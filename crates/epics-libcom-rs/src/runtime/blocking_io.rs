//! Blocking socket ⇄ `AsyncRead`/`AsyncWrite` adapters: the byte source for
//! every reactor-free driver in the workspace.
//!
//! ```text
//!   socket --read--> reader pump thread --Vec<u8> chunks--> ChannelReader
//!                                                              |
//!                                       whatever drives the protocol future
//!                                                              |
//!   socket <--write-- writer pump thread <--framed bytes-- ChannelWriter
//! ```
//!
//! # Why this lives in `epics-base-rs` and not in a protocol crate
//!
//! It was written once, inside `epics-pva-rs`'s blocking **server** driver, and
//! the obvious next move was to promote it within that crate so the PVA client
//! could reach it too. Measured, that destination is wrong:
//! `epics-ca-rs` does not depend on `epics-pva-rs` and must not — the only
//! crate that depends on both is `epics-bridge-rs`, which sits *above* them
//! (`doc/calink-rtems-design.md` §3.3). A primitive promoted inside
//! `epics-pva-rs` is one the CA client structurally cannot call, so the next CA
//! increment writes a third copy — exactly the outcome "one seam, two callers"
//! exists to prevent.
//!
//! So it lands here, beside the rest of its family: `runtime::task::spawn`,
//! `block_on_sync`/`park_on`, `StackSizeClass`, `spawn_dedicated_thread`,
//! `enter_ioc_thread`. Every protocol crate can reach it, and none of them owns
//! it.
//!
//! # The seam is the byte source, not the frame pipeline
//!
//! Nothing here parses anything. Both pumps move `Vec<u8>` and neither knows
//! whether the bytes are PVA frames, CA messages, or noise; the protocol future
//! on the other side of the adapters is untouched and uncompiled-differently.
//! That is what makes a driver built on this primitive arguable from the hosted
//! driver's own tests: same parser, same `select!`, same handlers, different
//! implementors of two `dyn` traits.
//!
//! # Two facts that must not be re-derived
//!
//! * **No fd dup.** The read and write roles come from **one** descriptor
//!   shared through an `Arc`, via `impl Read for &TcpStream` /
//!   `impl Write for &TcpStream` — never `try_clone`. `try_clone` is
//!   `fcntl(F_DUPFD_CLOEXEC)`, and on RTEMS 6 that cannot work for a socket:
//!   RTEMS's `fcntl` has no `F_DUPFD_CLOEXEC` case at all
//!   (`cpukit/libcsupport/src/fcntl.c:146-220` falls to
//!   `default: errno = EINVAL`), and even plain `F_DUPFD` fails because
//!   `duplicate_iop` calls the file's `open_h` while rtems-libbsd installs
//!   `rtems_bsd_sysgen_nodeops` on every socket. Measured on the target: `dup`,
//!   `F_DUPFD` and `F_DUPFD_CLOEXEC` all fail on a socket while `F_DUPFD` on
//!   `/dev/console` succeeds. A caller that reaches for `try_clone` compiles
//!   and fails at runtime on target only.
//! * **A blocking write needs a deadline, not a per-syscall timeout.**
//!   `SO_SNDTIMEO` bounds each `write` syscall, so a peer that accepts one byte
//!   per tick never trips it and holds the pump thread indefinitely — and a
//!   target that does not implement the option at all (VxWorks 7) has no bound
//!   whatever. [`write_frame_deadline`] therefore waits for writability against
//!   its own deadline and sets no socket option.
//!
//! # Lifecycle: a pump you cannot spawn without holding the thing that ends it
//!
//! [`spawn_reader_pump`] and [`spawn_writer_pump`] each return an adapter *and*
//! a guard, and there is no way to obtain the former without the latter. The
//! guards' `Drop` is what retires the threads, which is what makes every exit
//! path — clean return, `?`, and a panic unwinding through the caller — covered
//! without any cleanup written on an error branch.
//!
//! The two guards end their threads differently because the threads park
//! differently:
//!
//! | guard | how its thread is parked | how the guard returns it |
//! |---|---|---|
//! | [`ReaderPumpGuard`] | inside a blocking `read` behind an effectively-infinite `SO_RCVTIMEO` | `shutdown(Shutdown::Both)` on the shared socket, then `join` |
//! | [`WriterPumpGuard`] | inside `recv()` on the frame channel | drop the only strong sender, then `join` |
//!
//! A caller that needs a specific teardown *order* — writer down first so
//! frames emitted on the way out reach the wire, then reader — gets it by
//! dropping the guards in that order, or by declaring them in the reverse of
//! it.
//!
//! The reader guard's row is a POSIX contract: `shutdown` waking a thread
//! parked in a blocking `read` is what unix (RTEMS included) provides and
//! Windows does not (measured, PR #56 CI 2026-07-24 — the parked read
//! outlived a 120 s bound). That is why `exec_backend`, the only
//! configuration that runs these pumps in production, refuses Windows at
//! compile time (`lib.rs`), and why the tests asserting this contract are
//! `#[cfg(unix)]`.
//!
//! # Where the async goes
//!
//! [`block_on_sync`] is the single bridge, in both pumps. On a bare thread it
//! parks; on a multi-thread runtime worker it hands the worker off first. It is
//! **not** `blocking_send`, which panics inside a runtime context and would
//! make this module unusable from a hosted worker.
//!
//! # Before the pumps: the dial
//!
//! A reactor-free driver cannot `await` a connect either, so the blocking
//! `connect` needs a thread just as the two pumps do — and for the same reason,
//! at the same band. [`DialPool`] owns those threads. It is here rather than in
//! a protocol crate for the reason the pumps are: `epics-ca-rs` and
//! `epics-pva-rs` both dial and neither may depend on the other.

// RTEMS-EXEC-MODEL-ALLOW(1): `one_descriptor_serves_both_pumps` asserts both
// pump directions concurrently from the async side, which needs the
// multi-thread tokio flavor; it runs and passes in the feature-ON suite (the
// pumps themselves are std threads, the tokio runtime only hosts the
// assertions).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::ReadBuf;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::runtime::task::{StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread};
use crate::runtime::worker_pool::{Job, SetLease, Worker, WorkerPool, WorkerRole};

/// One blocking read, sized to match the frame readers that consume it so the
/// byte arrival pattern is the hosted one.
pub const DEFAULT_READ_CHUNK: usize = 4096;

/// The `FIONREAD` ioctl request — bytes pending in the socket receive queue.
/// C `rsrv`'s batch-up gate: hold accumulated replies while this is `> 0`,
/// flush at `0` (`camsgtask.c:55`, `cast_server.c:272`), and libca's flow
/// control input on the client side (`tcpiiu.cpp:544`).
///
/// The `libc` crate exposes `FIONREAD` for hosted Unix but omits it for
/// `armv7-rtems-eabihf`, so the RTEMS value is supplied here. RTEMS newlib
/// defines it in `sys/rtems/include/sys/filio.h` as `_IOR('f', 127, int)`;
/// `sys/ioccom.h` in the same tree encodes
/// `_IOR(g,n,t) = IOC_OUT | (sizeof(t) << 16) | (g << 8) | n` with
/// `IOC_OUT = 0x40000000`. For a 4-byte `int` that is
/// `0x40000000 | (4 << 16) | ('f' << 8) | 127 = 0x4004_667F` — the same value
/// the `libc` crate hardcodes for the whole BSD family (`unix/bsd/mod.rs`),
/// which C `rsrv` runs on RTEMS in production. Pending on-target runtime
/// verification at the QEMU/BSP phase; a wrong value only makes the `ioctl`
/// error, and every caller then flushes (C's own `status < 0` branch),
/// degrading to per-datagram / per-iteration flushing — never a hang or a
/// crash. (Candidate for an upstream `libc` newlib/rtems binding so this
/// local definition can later be dropped.)
#[cfg(all(unix, not(target_os = "rtems")))]
const FIONREAD_REQUEST: libc::c_ulong = libc::FIONREAD as libc::c_ulong;
#[cfg(target_os = "rtems")]
const FIONREAD_REQUEST: libc::c_ulong = 0x4004_667F;

/// Bytes pending in the socket receive queue via `FIONREAD`.
///
/// **One owner for the whole workspace.** Two callers need this exact
/// question answered and they are on opposite sides of the protocol: C
/// `rsrv`'s batch-up gate holds accumulated replies while this is `> 0` and
/// flushes at `0` (`camsgtask.c:52-67`, `cast_server.c:268-281`), and libca's
/// `tcpiiu::bytesArePendingInOS()` is the sole input to client flow control
/// (`tcpiiu.cpp:544-567`). They were two implementations — the server's here,
/// the client's a bare `libc::FIONREAD` that does not exist on
/// `armv7-rtems-eabihf` at all — which is one implementation too many for a
/// constant whose RTEMS value had to be derived from newlib headers by hand.
///
/// On any `ioctl` error this returns `Err`, and every caller treats that as
/// "flush now" / "nothing pending" — matching C's `status < 0` branch — so an
/// absent or wrong FIONREAD never coalesces (byte-correct, just unbatched),
/// never latches flow control on, and never hangs.
#[cfg(unix)]
pub fn pending_bytes<F: std::os::fd::AsRawFd>(sock: &F) -> io::Result<usize> {
    let mut n: libc::c_int = 0;
    // SAFETY: `as_raw_fd()` is a valid open socket fd; FIONREAD writes one
    // `c_int` count through the out-pointer, whose type and size match.
    let rc = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            FIONREAD_REQUEST as _,
            &mut n as *mut libc::c_int,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n.max(0) as usize)
}

#[cfg(not(unix))]
pub fn pending_bytes<F>(_sock: &F) -> io::Result<usize> {
    // No FIONREAD off Unix (RTEMS and the host CI are both Unix-family). Report
    // "unavailable" so callers flush every iteration — never coalesce — which
    // is byte-correct, just unbatched.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "FIONREAD unavailable on this platform",
    ))
}

/// A blocking socket op hit its `SO_RCVTIMEO`/`SO_SNDTIMEO`.
///
/// Unix reports the expiry as `WouldBlock`, some platforms as `TimedOut`.
pub fn is_socket_timeout(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Announce a pump thread that did not end normally.
///
/// The guards below make a loss *survivable* — the connection is torn down
/// however a pump ends. They do not make it *visible*, and a process that has
/// lost a thread but reads exactly like a healthy one is what closes here. The
/// loss that reaches this function is a pump that panicked; a pooled worker is
/// never *created* per connection, so the old "could not be created" loss is
/// gone with the per-connection spawn.
///
/// Through `errlog` and not `tracing` alone: `errlog_sev_printf` reaches the
/// console whatever the log configuration — including an RTEMS console whose
/// subscriber is the in-tree one — and printing it is what a C IOC does.
fn pump_thread_lost(role: &str, label: &str, what: &str) {
    crate::runtime::log::errlog_sev_printf(
        crate::runtime::log::ErrlogSevEnum::Major,
        &format!(
            "{label}: the {role} thread {what}; this connection is being torn \
             down. Other connections are unaffected."
        ),
    );
    warn!(label, role, what, "blocking socket pump: a thread was lost");
}

// ---------------------------------------------------------------------------
// Dial side
// ---------------------------------------------------------------------------

/// How many dial threads one [`DialPool`] may ever create.
///
/// The bound is on *creations for the life of the process*, not on threads
/// alive at an instant, because that is the resource that was being consumed:
/// every `std::thread` leaks 128 B on RTEMS permanently (its TLS key is freed
/// before the key's destructor runs), so a dial that spawns per attempt leaks
/// per attempt. A pool whose workers never retire creates at most this many,
/// ever — the leak becomes a one-off 4 × 128 B, whatever the redial cadence.
///
/// Four, not one: a worker is occupied for as long as its `connect` blocks, and
/// a SYN-blackholed peer holds one for the whole OS connect ladder long after
/// the awaiting side gave up at its own bound. The ladder that bounds a worker
/// is the *target's*, not the host's: on `armv7-rtems-eabihf` libbsd ends an
/// unanswered handshake at `TCPTV_KEEP_INIT` (`75 * hz`), measured at 75 s,
/// while a Linux host runs `tcp_syn_retries` out to ~130 s. This pool exists
/// for the RTEMS target, so 75 s is the figure its sizing is reasoned against.
/// One worker would let a single unreachable peer head-of-line-block
/// every other dial in the process; four keeps distinct in-flight dials
/// independent in normal operation. The cost is four `Small` stacks
/// (4 × 256 KiB on `armv7-rtems-eabihf`), and only if four dials were ever
/// concurrently in flight — a client that only ever dials one server at a time
/// creates exactly one worker and reuses it forever.
///
/// Past the bound, dials queue. That is not a failure mode that needs its own
/// handling: a queued request is still under the caller's own timeout, so it
/// fails at that deadline exactly as an in-flight one would, and a worker that
/// later reaches a request whose caller has gone opens no socket at all.
pub const MAX_DIAL_WORKERS: usize = 4;

/// One dial handed to a worker: where to connect, and where the result goes.
struct DialRequest {
    target: SocketAddr,
    reply: oneshot::Sender<io::Result<TcpStream>>,
}

/// Everything the pool mutates, under one lock.
///
/// The three counts answer one question — *is a worker owed?* — and are kept in
/// the shape that makes the answer exact: `workers - busy` is available, and a
/// request is covered iff the available ones outnumber the queue.
struct DialQueue {
    /// Requests no worker has taken yet.
    pending: VecDeque<DialRequest>,
    /// Workers holding a request. Counting the *busy* ones rather than the
    /// parked ones is load-bearing: a worker between its `connect` and its park
    /// is neither, and counting parked workers would make it read as
    /// unavailable — so a caller woken by that very worker's reply would create
    /// a second one it does not need. The busy count is released *before* the
    /// reply is sent, so a woken caller always sees its worker as available.
    busy: usize,
    /// Workers created. Only ever decremented when a spawn *fails*: a worker
    /// that exists never exits, which is the whole point.
    workers: usize,
}

/// A bounded, permanent set of threads that own this role's blocking TCP
/// dials.
///
/// # Why the dial needs a thread at all
///
/// The connect is a blocking syscall and every caller is a task. On the exec
/// backend a task runs on a cooperative callback-band worker shared with every
/// other future on its band, so connecting inline parks the band for the whole
/// attempt — measured exactly there (gdb all-thread dump, host-linux
/// `realtime-pva-ioc`): one unanswering name server starved every future on Medium
/// for ~40 s per attempt. So the connect goes to a thread and the caller parks
/// on a oneshot instead.
///
/// # Why the threads are permanent
///
/// The obvious shape — one transient thread per dial — is unbounded in thread
/// *creations*, and creations are what cost on RTEMS (see [`MAX_DIAL_WORKERS`]).
/// A search engine whose name server is down redials roughly every 10 s for as
/// long as the IOC runs, so "transient, one per attempt" is a leak with no
/// ceiling. Making the workers permanent and reusing them removes the family
/// rather than capping it: after the first dial of each concurrency level there
/// is nothing left to create.
///
/// # What a worker owes the socket it opens
///
/// A worker is the **single finalizer** for every socket it opens. If the
/// caller gave up (timed out, or its future was dropped) the oneshot send fails
/// and the returned `TcpStream` is dropped right there, closing the fresh
/// socket. A worker that reaches a request whose caller is already gone skips
/// the connect entirely, so a backlog built up behind a blackholed peer costs
/// no sockets at all.
///
/// # Where the timeout is *not*
///
/// The worker issues a plain blocking [`TcpStream::connect`] — the CA client's
/// proven on-target dial, C parity with `tcpiiu.cpp`'s blocking `::connect()`,
/// and a thread that owns its blocking needs no poll machinery. The
/// application-level bound belongs to the awaiting side, which holds the
/// [`oneshot::Receiver`] this returns and is free to wrap it in
/// `runtime::task::timeout`. Do not add a bound here: the two are deliberately
/// split, and collapsing them puts the application deadline back inside a
/// syscall that cannot honour it.
pub struct DialPool {
    /// OS thread-name stem; workers are `"{name_prefix} {index}"`. Keep it
    /// short — RTEMS truncates thread names at 16 bytes.
    name_prefix: &'static str,
    /// The band every worker enters. Dials belong to the band of the pumps
    /// they precede, so this is per-role and is why the pool is not global.
    priority: ThreadPriority,
    queue: Mutex<DialQueue>,
    work: Condvar,
}

impl DialPool {
    /// Declare a role's dial pool. `const` so it can be a `static`: a pool is
    /// per-role and lives as long as the process, so a caller needs no `Arc`
    /// and no lazy initialiser.
    pub const fn new(name_prefix: &'static str, priority: ThreadPriority) -> Self {
        Self {
            name_prefix,
            priority,
            queue: Mutex::new(DialQueue {
                pending: VecDeque::new(),
                busy: 0,
                workers: 0,
            }),
            work: Condvar::new(),
        }
    }

    /// Threads this pool has created — never more than [`MAX_DIAL_WORKERS`].
    ///
    /// The bound made observable: this is the number the per-attempt shape grew
    /// without limit.
    pub fn worker_count(&self) -> usize {
        self.lock().workers
    }

    /// Requests waiting for a worker, and workers currently inside a dial.
    ///
    /// The other half of the bound: `worker_count()` alone cannot distinguish
    /// "four workers, nothing queued" from "four workers, every one pinned and
    /// a fifth dial waiting" — which is the state
    /// [`MAX_DIAL_WORKERS`] exists to produce and the
    /// only state in which the queueing it documents is observable.
    pub fn queue_depth(&self) -> (usize, usize) {
        let q = self.lock();
        (q.pending.len(), q.busy)
    }

    /// Submit a dial. The returned receiver resolves with whatever the worker's
    /// `connect` returned.
    ///
    /// The error is a thread-creation failure, and only that: it is returned
    /// *before* the request is queued, so a caller that sees it knows no dial is
    /// pending on its behalf.
    pub fn dial(
        &'static self,
        target: SocketAddr,
    ) -> io::Result<oneshot::Receiver<io::Result<TcpStream>>> {
        let (reply, rx) = oneshot::channel();
        let req = DialRequest { target, reply };

        let mut q = self.lock();
        // Each queued request already claims one available worker, so this
        // request is covered only if the available ones outnumber the queue.
        if q.pending.len() + q.busy < q.workers || q.workers >= MAX_DIAL_WORKERS {
            q.pending.push_back(req);
            drop(q);
            self.work.notify_one();
            return Ok(rx);
        }

        // Create the worker *before* queueing, so a spawn failure leaves the
        // pool exactly as it found it and the caller keeps its error.
        let index = q.workers;
        q.workers += 1;
        drop(q);
        if let Err(e) = spawn_dedicated_thread(
            format!("{} {index}", self.name_prefix),
            self.priority,
            StackSizeClass::Small,
            move || self.worker_loop(),
        ) {
            self.lock().workers -= 1;
            return Err(e);
        }
        self.lock().pending.push_back(req);
        self.work.notify_one();
        Ok(rx)
    }

    /// A worker's whole life: take a request, connect, hand the socket back.
    ///
    /// Never returns. See the type docs for why that is the fix rather than an
    /// oversight.
    fn worker_loop(&self) -> ! {
        loop {
            let req = {
                let mut q = self.lock();
                loop {
                    if let Some(req) = q.pending.pop_front() {
                        q.busy += 1;
                        break req;
                    }
                    // No lost wakeup to worry about: every worker re-reads
                    // `pending` under this lock before parking, so a request
                    // queued while this one was still running is seen here.
                    q = self.work.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            // The caller gave up while this request sat in the queue. Opening a
            // socket nobody can receive would only make this worker its
            // finalizer for no reason.
            let dialed = (!req.reply.is_closed()).then(|| TcpStream::connect(req.target));
            // Release the slot *before* replying: the caller this reply wakes
            // may dial again immediately, and it must see this worker as
            // available rather than create a second one.
            self.lock().busy -= 1;
            if let Some(dialed) = dialed {
                // Single finalizer: a failed send drops the `TcpStream` here,
                // which closes the socket this worker opened.
                let _ = req.reply.send(dialed);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, DialQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Reader side
// ---------------------------------------------------------------------------

/// `AsyncRead` over a channel of byte chunks — the blocking stand-in for a
/// socket read half.
///
/// **Cancel-safety** is the whole point of the `cur`/`pos` pair and is why this
/// type exists at all rather than a channel being read inline. A frame reader
/// used directly as a `select!` arm survives losing that race because its
/// accumulated bytes live *outside* it. This adapter has the same property: a
/// chunk leaves the channel only when `poll_recv` returns `Ready`, and a
/// partially-copied chunk stays in `cur`/`pos` across as many dropped
/// `poll_read` futures as the caller likes. A lost race consumes nothing.
pub struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    /// The chunk currently being handed out, and how much of it has gone.
    cur: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    /// Build an adapter over an existing chunk channel.
    ///
    /// Public because a caller may want the adapter without a socket behind it
    /// — a test double, or a byte source that is not a `TcpStream`. The paired
    /// [`spawn_reader_pump`] is what a socket-backed caller wants.
    pub fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
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
        // No room offered: report "nothing filled" without taking anything out
        // of the channel. Consuming here would be the one way this adapter
        // could lose bytes.
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
                    // An empty chunk is not an EOF marker; skip it rather than
                    // letting it read as one.
                    if chunk.is_empty() {
                        continue;
                    }
                    me.cur = chunk;
                    me.pos = 0;
                }
                // Every sender gone = the reader thread ended (EOF, read error,
                // or RCVTIMEO). Zero bytes filled is what a frame reader turns
                // into its own peer-closed error — the existing hosted EOF
                // path, unchanged.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Read loop. Ends on EOF, read error, or a read that outlives `read_timeout`;
/// dropping `tx` on the way out is the EOF signal to the adapter.
///
/// The wait is [`wait_readable`], not the socket's own `SO_RCVTIMEO`, because
/// the descriptor is non-blocking — see [`own_blocking_mode`] for why it has to
/// be. `read_timeout` is the same value the option carried, applied per read
/// exactly as the option applied it, so a connection ends on a silent peer at
/// the same point it did before.
fn reader_pump(
    sock: Arc<TcpStream>,
    tx: mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    label: String,
    read_timeout: Option<Duration>,
) {
    // `impl Read for &TcpStream`: one shared descriptor, no `try_clone`.
    let mut sock = &*sock;
    let mut chunk = vec![0u8; chunk_size];
    loop {
        match wait_readable(sock, read_timeout.map(|t| Instant::now() + t)) {
            Ok(true) => {}
            Ok(false) => {
                debug!(label, "blocking reader: receive timeout, ending connection");
                break;
            }
            Err(e) => {
                debug!(label, error = %e, "blocking reader: wait failed");
                break;
            }
        }
        let n = match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Readiness that yielded nothing: back to the wait, which re-arms
            // the same bound, so this cannot spin.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) if is_socket_timeout(e.kind()) => {
                debug!(label, "blocking reader: receive timeout, ending connection");
                break;
            }
            Err(e) => {
                debug!(label, error = %e, "blocking reader: read failed");
                break;
            }
        };
        // The house sync-over-async primitive: parks this thread (no runtime
        // entered) or hands the worker off (hosted). NOT `blocking_send`.
        if !matches!(block_on_sync(tx.send(chunk[..n].to_vec())), Ok(Ok(()))) {
            break;
        }
    }
}

/// The spawned reader pump, woken and joined on **every** exit path.
///
/// # Invariant
///
/// MUST: once the reader pump has been spawned, it is woken and joined before
/// its owner returns — clean return, `?`, or a panic unwinding out of the
/// caller.
///
/// # The defect this closes
///
/// A writer-spawn failure used to `?` out with the reader already running,
/// leaving it parked in `read` behind an `SO_RCVTIMEO` that a PVA `op_timeout`
/// makes effectively infinite (~64,000 s by default), holding its socket and
/// its descriptor for the life of the IOC. The connection slot was returned
/// correctly, which is exactly what made the leak invisible: the connection
/// count looked healthy while descriptors drained away.
///
/// Owning the handle in a guard, rather than calling cleanup on the error
/// branch, is what makes the leak unexpressible: there is no way to have
/// spawned the reader without also holding the value that joins it. The same
/// applies to the panic path, which no error-branch cleanup could have covered.
pub struct ReaderPumpGuard {
    /// The same descriptor the pump reads from. Owning an `Arc` rather than
    /// borrowing is load-bearing: waking a pump that has already ended must be
    /// a no-op on a still-open fd, never a `shutdown` of an fd number the OS
    /// has since handed to someone else.
    sock: Arc<TcpStream>,
    label: String,
    /// The pooled job running `reader_pump`. Joining it returns the worker to
    /// its pool; the worker itself is not retired, only the job.
    job: Option<Job>,
}

impl Drop for ReaderPumpGuard {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            // The pump's `read` is parked behind an effectively-infinite
            // timeout, so the socket has to be shut to return it. `ENOTCONN`
            // when the peer has already gone: there was nothing to wake, which
            // is not a failure of anything.
            let _ = self.sock.shutdown(Shutdown::Both);
            // The join result is the only place a panicked pump is ever
            // reported: `reader_pump` returns `()`, so an `Err` here means it
            // unwound, and the connection's own error will be a bland
            // channel-closed rather than the cause. Discarding it left the two
            // unlinkable.
            if job.join().is_err() {
                pump_thread_lost("reader", &self.label, "panicked");
            }
        }
    }
}

/// Drive `sock`'s read half on a pooled `worker`, yielding the `AsyncRead`
/// half of the seam and the guard that retires the job.
///
/// Infallible: the thread already exists — it is the leased `worker` — so there
/// is no creation to fail. Admission failure now lives in
/// [`WorkerPool::acquire`], which is where it belongs.
///
/// `queue_depth` is the chunk channel's depth. **One** is the faithful choice
/// for a demand-driven frame reader — one read per poll, each frame dispatched
/// fully before the next read — because it reproduces that with at most one
/// chunk of read-ahead, which the kernel receive buffer already provides. A
/// larger depth lets a fast peer queue chunks while a slow consumer blocks: a
/// behaviour change, not an optimisation.
pub fn spawn_reader_pump(
    worker: Worker,
    sock: Arc<TcpStream>,
    label: &str,
    chunk_size: usize,
    queue_depth: usize,
) -> (ChannelReader, ReaderPumpGuard) {
    // What the caller configured, read back rather than passed in, so this
    // signature is unchanged: the socket already carries the read bound, and
    // `SO_RCVTIMEO` stops being the mechanism that applies it without ceasing
    // to be where the value lives. `None` — never set, or a target whose
    // getter declines — polls with no deadline, which is what a socket with no
    // `SO_RCVTIMEO` did before.
    let read_timeout = sock_read_timeout(&sock);
    spawn_reader_pump_with_timeout(worker, sock, label, chunk_size, queue_depth, read_timeout)
}

fn sock_read_timeout(sock: &TcpStream) -> Option<Duration> {
    sock.read_timeout().ok().flatten()
}

fn spawn_reader_pump_with_timeout(
    worker: Worker,
    sock: Arc<TcpStream>,
    label: &str,
    chunk_size: usize,
    queue_depth: usize,
    read_timeout: Option<Duration>,
) -> (ChannelReader, ReaderPumpGuard) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(queue_depth);
    let pump_sock = sock.clone();
    let pump_label = label.to_string();
    let job = worker.run(move || reader_pump(pump_sock, tx, chunk_size, pump_label, read_timeout));
    (
        ChannelReader::new(rx),
        ReaderPumpGuard {
            sock,
            label: label.to_string(),
            job: Some(job),
        },
    )
}

// ---------------------------------------------------------------------------
// Writer side
// ---------------------------------------------------------------------------

/// Wake slot for a `poll_write` that found the frame channel full. The writer
/// pump wakes it after each frame it takes, which is the moment room appears.
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

/// `AsyncWrite` over a channel of frames — the blocking stand-in for a socket
/// write half.
///
/// Holds a [`mpsc::WeakSender`], never a strong one, and that is load-bearing
/// rather than tidiness. This adapter is typically owned by a task that is
/// *aborted*, not joined, when the connection ends, so the moment its last
/// strong sender drops is not a moment the owner controls. With only a weak
/// handle here, [`WriterPumpGuard`]'s sender is the sole thing keeping the
/// channel open, and dropping it ends the pump deterministically instead of
/// whenever the runtime gets round to reaping an aborted task.
pub struct ChannelWriter {
    tx: mpsc::WeakSender<Vec<u8>>,
    room: Arc<WriteRoom>,
}

fn write_closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the writer pump thread has ended",
    )
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
        // `tx` drops here. Nothing in this adapter holds a strong sender across
        // a suspension, which is what makes the guard's drop decisive.
    }

    /// Frames are flushed by the writer pump as it takes them; there is no
    /// buffer here to push.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// `POLLOUT` and `MSG_DONTWAIT` for this target.
///
/// Deliberately not `libc::POLLOUT` / `libc::MSG_DONTWAIT`. On
/// `armv7-rtems-eabihf` the `libc` crate glob-re-exports both
/// `unix/newlib/arm` and `unix/newlib/rtems`, and those two modules define
/// these names with **different values** — `POLLOUT` `0x10` against `0x0004`,
/// `MSG_DONTWAIT` `4` against `0x80`, 16 names colliding that way in total. A
/// glob-versus-glob collision is an ambiguity error at the use site, so naming
/// them through `libc` does not compile there; and taking the `arm` values
/// would be wrong regardless, because RTEMS's stack is libbsd and the FreeBSD
/// values are the true ones (`sys/poll.h`, `sys/socket.h`). Stated here for the
/// same reason [`FIONREAD_REQUEST`] above is stated: the constant is derived
/// from the target's own headers rather than from whichever module the glob
/// happened to win.
///
/// Every other Unix — the hosted hosts and `*-wrs-vxworks*`, where one module
/// defines each name — takes `libc`'s.
#[cfg(target_os = "rtems")]
const POLLOUT_EVENT: libc::c_short = 0x0004;
#[cfg(target_os = "rtems")]
const POLLIN_EVENT: libc::c_short = 0x0001;
#[cfg(target_os = "rtems")]
const SEND_DONTWAIT: libc::c_int = 0x0080;
#[cfg(all(unix, not(target_os = "rtems")))]
const POLLOUT_EVENT: libc::c_short = libc::POLLOUT;
#[cfg(all(unix, not(target_os = "rtems")))]
const POLLIN_EVENT: libc::c_short = libc::POLLIN;
#[cfg(all(unix, not(target_os = "rtems")))]
const SEND_DONTWAIT: libc::c_int = libc::MSG_DONTWAIT;

/// Put `sock` in non-blocking mode, so that no syscall this module issues on it
/// can park regardless of which flags and options the target honours.
///
/// This module owns the descriptor's blocking mode; that ownership is what the
/// bound is made of. Both directions are gated by a `poll` against the caller's
/// own deadline ([`wait_readable`], [`wait_writable`]), so the mode is not a
/// performance choice but the thing that makes "the syscall returns" true by
/// construction instead of true wherever `MSG_DONTWAIT` or `SO_SNDTIMEO`
/// happens to be implemented.
///
/// C reaches the same place the same way: `setNonBlock(fd, 1)` at connect under
/// `USE_POLL` (`drvAsynIPPort.c:511`), with a poll on reads as well as writes.
/// Which is also the evidence that the call is available on the embedded
/// targets — it is `ioctl(FIONBIO)`, the one socket control C already relies on
/// there, not one of the options VxWorks answers `ENOPROTOOPT` to.
///
/// Windows keeps blocking sockets and its `SO_SNDTIMEO`/`SO_RCVTIMEO`, which it
/// does implement; the `not(unix)` arms of both waits are built on them.
#[cfg(unix)]
fn own_blocking_mode(sock: &TcpStream) -> io::Result<()> {
    sock.set_nonblocking(true)
}

#[cfg(not(unix))]
fn own_blocking_mode(_sock: &TcpStream) -> io::Result<()> {
    Ok(())
}

/// Wait until `sock` has a byte to read or has hit EOF, or `deadline` passes.
/// `Ok(true)` = readable, `Ok(false)` = the deadline passed with it still empty.
/// `None` waits with no deadline, which is what an unconfigured socket did
/// before.
///
/// The read-side twin of [`wait_writable`], and it exists for the same reason:
/// with the descriptor non-blocking, a `read` cannot park, so the bound has to
/// come from here. It replaces `SO_RCVTIMEO` as the *mechanism* while keeping
/// it as the *value* — callers still say how long a read may take, and
/// [`drive_socket_blocking`] still sets the option for the `not(unix)` arm.
///
/// `POLLHUP` also returns `Ok(true)`: the read that follows returns 0 and the
/// pump ends on its existing EOF path, which is how a `shutdown` still wakes a
/// waiting reader now that no `read` is parked for it to interrupt.
#[cfg(unix)]
fn wait_readable(sock: &TcpStream, deadline: Option<Instant>) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    loop {
        let ms = match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int
            }
            None => -1,
        };
        let mut fds = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: POLLIN_EVENT,
            revents: 0,
        };
        // SAFETY: one initialised `pollfd` whose `fd` is this borrowed socket's
        // and stays open for the call; `poll` reads `fd`/`events` and writes
        // only `revents`.
        let rc = unsafe { libc::poll(&mut fds, 1, ms) };
        if rc > 0 {
            return Ok(true);
        }
        if rc == 0 {
            return Ok(false);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        // `EINTR`: the remaining time is recomputed at the top, so a signal
        // storm cannot extend the bound.
    }
}

/// Non-Unix arm: Windows implements `SO_RCVTIMEO` and keeps a blocking socket,
/// so the read that follows carries its own bound and this only arms it.
#[cfg(not(unix))]
fn wait_readable(sock: &TcpStream, deadline: Option<Instant>) -> io::Result<bool> {
    let Some(d) = deadline else {
        sock.set_read_timeout(None)?;
        return Ok(true);
    };
    let remaining = d.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    sock.set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
    Ok(true)
}

/// Wait until `sock` will accept at least one byte, or `deadline` passes.
/// `Ok(true)` = writable, `Ok(false)` = the deadline passed with it still full.
///
/// Half of what makes [`write_frame_deadline`]'s bound hold by construction:
/// the wait belongs to this module, so no socket option is load-bearing and a
/// target that implements none of them is bounded exactly as one that
/// implements them all. `POLLERR`/`POLLHUP` also return `Ok(true)`, so the send
/// that follows reports the real errno instead of this function inventing one.
#[cfg(unix)]
fn wait_writable(sock: &TcpStream, deadline: Instant) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        // Rounded up to 1 ms so a sub-millisecond remainder waits instead of
        // spinning, and clamped so a very long deadline still fits `poll`'s
        // `c_int` milliseconds.
        let ms = remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
        let mut fds = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: POLLOUT_EVENT,
            revents: 0,
        };
        // SAFETY: one initialised `pollfd` whose `fd` is this borrowed socket's
        // and stays open for the call; `poll` reads `fd`/`events` and writes
        // only `revents`.
        let rc = unsafe { libc::poll(&mut fds, 1, ms) };
        if rc > 0 {
            return Ok(true);
        }
        if rc == 0 {
            return Ok(false);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
        // `EINTR`: the remaining time is recomputed at the top, so a signal
        // storm cannot extend the bound.
    }
}

/// Hand as much of `buf` to the socket as it will take **without parking**,
/// however many bytes that is.
///
/// The other half of the bound, and the half that is easy to get wrong: a
/// blocking `write` on a stream socket does not return a short count when the
/// send buffer fills, it waits until the *whole* buffer is queued
/// (`tcp_sendmsg` parks in `sk_stream_wait_memory`). So waiting for `POLLOUT`
/// first is not enough on its own — the very next `write` re-enters the same
/// unbounded wait one byte later. The socket is non-blocking for as long as it
/// is live ([`own_blocking_mode`]), so the send takes what there is room for and
/// returns; `MSG_DONTWAIT` rides on top as the per-call form of the same ask.
///
/// That mode is on the file description the reader pump shares (see the module
/// docs on why it is shared and not `dup`ed), which is what [`wait_readable`]
/// exists for: the reader polls before reading rather than parking in `read`,
/// so sharing the description with a non-blocking writer costs it nothing.
///
/// A full buffer surfaces as `EAGAIN`/`WouldBlock`, which returns the caller to
/// [`wait_writable`] and therefore to the deadline.
///
/// `SIGPIPE` needs no flag here: Rust's startup sets it to `SIG_IGN` on every
/// Unix target, so a send to a closed peer returns `EPIPE`.
///
/// # The flag is the fast path, not the guarantee
///
/// It cannot be the guarantee, because a target may ignore it. XNU's `sosend`
/// decides whether to sleep from `so_state & SS_NBIO` and its own internal
/// `MSG_NBIO`, and `MSG_DONTWAIT` reaches it only as the sockbuf-lock wait
/// hint, so on Darwin the send parked and the deadline was left riding on
/// whatever `SO_SNDTIMEO` the caller had armed — measured, macOS CI
/// 2026-07-27, `doc/darwin-send-dontwait-gap.md`. What makes the send return
/// on every target is [`own_blocking_mode`]. The flag stays because where it
/// *is* honoured it saves the loop a `poll` on the common path where the
/// socket has room.
#[cfg(unix)]
fn write_some(sock: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;

    // SAFETY: `buf` is a valid initialised slice borrowed for the call, and
    // `as_raw_fd()` is this borrowed socket's open descriptor. `send` reads
    // `buf.len()` bytes from the pointer and writes nothing through it.
    let n = unsafe {
        libc::send(
            sock.as_raw_fd(),
            buf.as_ptr().cast(),
            buf.len(),
            SEND_DONTWAIT,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Non-Unix arm of the same two-part contract.
///
/// `poll` would mean `WSAPoll` and a Win32 dependency this crate does not
/// carry, and Windows *does* implement `SO_SNDTIMEO`. So arm it — from inside
/// this module, not from a caller — to the time the deadline has left: the send
/// that follows returns within `remaining`, and the loop ends the frame on the
/// next pass. The bound is still owned here, which is the property that
/// matters.
///
/// The blocking pumps are refused on Windows at compile time (`lib.rs`), so
/// this arm keeps the primitive's contract uniform where the module still
/// compiles rather than carrying production traffic.
#[cfg(not(unix))]
fn wait_writable(sock: &TcpStream, deadline: Instant) -> io::Result<bool> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    sock.set_write_timeout(Some(remaining.max(Duration::from_millis(1))))?;
    Ok(true)
}

#[cfg(not(unix))]
fn write_some(sock: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    let mut sock = sock;
    sock.write(buf)
}

/// Write one whole frame under a **deadline**, not merely a per-syscall
/// timeout.
///
/// A hosted writer bounds `write_all(&frame)` as a unit. A per-syscall socket
/// timeout bounds each `write` instead, so a peer that accepts one byte per
/// tick never trips it and holds the pump thread indefinitely — the exact
/// stuck-peer hazard the hosted timeout exists to prevent, on a resource (an OS
/// thread) that is scarcer on RTEMS than a task is on the host.
///
/// # The deadline holds on every target, by construction
///
/// This function owns its bound end to end. It takes over the socket's blocking
/// mode (`own_blocking_mode`, crate-private) so that no syscall below it can
/// park; `wait_writable` does every wait, against `deadline`. Between them there
/// is no call in this loop that can outlast `send_timeout`, and nothing a caller
/// does or fails to do can disarm it.
///
/// Owning the mode is what makes that true rather than nearly true. `write_some`
/// passing `MSG_DONTWAIT` is not enough on its own: XNU consults `SS_NBIO` and
/// its internal `MSG_NBIO` and ignores the flag a caller sends, so on Darwin the
/// send parked and the deadline was carried by whatever `SO_SNDTIMEO` the caller
/// happened to have armed — measured, macOS CI 2026-07-27, where the case that
/// armed none outlived a 20 s wait while its armed sibling ended on time. The
/// flag stays, because where it is honoured it saves the loop a `poll`, but it
/// is no longer what the guarantee rests on.
///
/// It used to lean on the caller having set `SO_SNDTIMEO`, which was the only
/// thing that returned control to this loop. That made the bound conditional on
/// a socket option, and VxWorks 7 does not implement it — `setsockopt` returns
/// `ENOPROTOOPT`, so on that target the deadline was silently absent and a peer
/// that accepted the connection and then stopped reading parked the pump with
/// nothing entitled to reclaim it
/// (`doc/vxworks-circuit-wedge-on-target-measurement.md` §5). An invariant that
/// one target can switch off is not an invariant; the wait is the caller's own
/// now, and `SO_SNDTIMEO` is not set anywhere in this module.
///
/// A partial write on expiry needs no repair: the caller ends the pump and the
/// connection is torn down, so nothing is ever written to this socket again.
pub fn write_frame_deadline(
    sock: &TcpStream,
    frame: &[u8],
    send_timeout: Duration,
) -> io::Result<()> {
    // Here, not at the call sites, so that no caller can be the one that forgot
    // — including a caller that reached this socket without going through
    // `drive_socket_blocking`. Idempotent, so the writer pump paying for it once
    // per frame costs an `ioctl` next to the `poll` and `send` it already makes.
    own_blocking_mode(sock)?;
    // `impl Write for &TcpStream`: rebind so `write`/`flush` have a mutable
    // place to borrow, without needing `&mut TcpStream` from the caller.
    let mut sock = sock;
    let deadline = Instant::now() + send_timeout;
    let mut off = 0;
    while off < frame.len() {
        // The one gate, ahead of every syscall, so every way round the loop is
        // bounded — a stalled peer, a trickling one, and an `Interrupted`
        // storm alike.
        if !wait_writable(sock, deadline)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "send deadline expired with the frame incomplete",
            ));
        }
        match write_some(sock, &frame[off..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "peer accepted no bytes",
                ));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // `EAGAIN` from the non-blocking send, or the non-Unix arm's socket
            // timeout: no progress, back round to the gate above.
            Err(e) if is_socket_timeout(e.kind()) => {}
            Err(e) => return Err(e),
        }
    }
    sock.flush()
}

/// Drain frames to the socket in order. Ends when the guard drops the last
/// strong sender, or on the first write error / send-deadline expiry.
///
/// Whichever of those ends it, it shuts the socket on the way out. A dead
/// writer means the connection is over, and the consumer must not wait up to a
/// heartbeat period to find that out — but the fix is the socket shutdown, not
/// an extra `select!` arm in the protocol loop: the reader pump's `read` then
/// returns 0 and the consumer unwinds down its existing EOF path, leaving the
/// protocol module and the hosted timing alone.
fn writer_pump(
    sock: Arc<TcpStream>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    room: Arc<WriteRoom>,
    send_timeout: Duration,
    label: String,
) {
    // `Ok(None)` = the guard let go of its sender; `Err(_)` = this thread
    // cannot block here at all. Both end the pump.
    while let Ok(Some(frame)) = block_on_sync(rx.recv()) {
        // A slot just opened; let a parked `poll_write` retry.
        room.wake();
        if let Err(e) = write_frame_deadline(&sock, &frame, send_timeout) {
            debug!(label, error = %e, "blocking writer: send failed, ending connection");
            break;
        }
    }
    // Whatever parked the producer, it must not stay parked on a dead writer.
    room.wake();
    // Uniform, not special-cased on *why* the pump ended: the only thing that
    // ends it is the connection being over. On the error paths this is what
    // retires the connection at once; on the normal path the owner is already
    // tearing down and repeats the same shutdown a moment later, harmlessly —
    // every frame this thread was given has been written before it gets here.
    let _ = sock.shutdown(Shutdown::Both);
}

/// The spawned writer pump and the only strong frame sender, retired together
/// on **every** exit path.
///
/// The sender lives here rather than beside the guard because the pump parks on
/// `rx.recv()` and leaves only when the last strong sender drops. A guard that
/// joined without dropping the sender would hang; keeping the two in one value
/// means the order cannot be got wrong, and does not depend on the declaration
/// order of two separate locals.
pub struct WriterPumpGuard {
    frames: Option<mpsc::Sender<Vec<u8>>>,
    label: String,
    /// The pooled job running `writer_pump`; joining it returns the worker.
    job: Option<Job>,
}

impl Drop for WriterPumpGuard {
    fn drop(&mut self) {
        // Decisive because it is the only strong sender — [`ChannelWriter`]
        // holds a weak handle. The pump drains what is queued, sees `None`, and
        // exits; on its way out it shuts the socket.
        drop(self.frames.take());
        if let Some(job) = self.job.take() {
            // Same reading as [`ReaderPumpGuard`]'s: an `Err` is a panicked
            // pump, and a pump that unwound with frames still queued dropped
            // them.
            if job.join().is_err() {
                pump_thread_lost("writer", &self.label, "panicked");
            }
        }
    }
}

/// Drive `sock`'s write half on a pooled `worker`, yielding the `AsyncWrite`
/// half of the seam and the guard that retires the job. Infallible for the same
/// reason as [`spawn_reader_pump`].
///
/// `queue_depth` follows the same reasoning as [`spawn_reader_pump`]'s: a
/// producer that emits one frame at a time and waits for it gets, at depth 1,
/// the same backpressure a blocking socket write would.
///
/// `send_timeout` bounds one whole frame and needs no cooperation from the
/// caller: [`write_frame_deadline`] owns the wait it is enforced by.
pub fn spawn_writer_pump(
    worker: Worker,
    sock: Arc<TcpStream>,
    label: &str,
    send_timeout: Duration,
    queue_depth: usize,
) -> (ChannelWriter, WriterPumpGuard) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(queue_depth);
    let room = Arc::new(WriteRoom::default());
    let adapter = ChannelWriter {
        tx: tx.downgrade(),
        room: room.clone(),
    };
    let pump_label = label.to_string();
    let job = worker.run(move || writer_pump(sock, rx, room, send_timeout, pump_label));
    (
        adapter,
        WriterPumpGuard {
            // The only strong sender moves into the guard, so it cannot be
            // dropped out of order with the join. `adapter` above already took
            // its weak handle.
            frames: Some(tx),
            label: label.to_string(),
            job: Some(job),
        },
    )
}

// ---------------------------------------------------------------------------
// Owning adapters: the shape a caller with no teardown thread of its own wants
// ---------------------------------------------------------------------------

/// A [`ChannelReader`] that owns its pump guard.
///
/// The server driver keeps its guards as locals because it *has* a thread that
/// outlives the protocol future and can drop them in a chosen order. A client
/// connection has no such thread: its reader and writer are tasks, and the
/// adapters are the only things the connection hands them. So for that shape
/// the guard rides *inside* the adapter, and the rule "you cannot hold the byte
/// source without holding the thing that retires its pump" holds there too.
pub struct GuardedReader {
    inner: ChannelReader,
    _guard: ReaderPumpGuard,
    /// The connection's worker-set lease, shared with [`GuardedWriter`]. The set
    /// returns to its pool only when both adapters — and both pump jobs they
    /// hold — are gone, which is exactly when the connection is over.
    _lease: Arc<SetLease>,
}

impl tokio::io::AsyncRead for GuardedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

/// A [`ChannelWriter`] that owns its pump guard. See [`GuardedReader`].
pub struct GuardedWriter {
    inner: ChannelWriter,
    _guard: WriterPumpGuard,
    /// The other strong reference to the connection's lease; see
    /// [`GuardedReader`].
    _lease: Arc<SetLease>,
}

impl tokio::io::AsyncWrite for GuardedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Everything a caller needs to set up on a socket before its pumps start.
#[derive(Clone, Debug)]
pub struct PumpConfig {
    /// `SO_RCVTIMEO`. NOT a shutdown mechanism — a protocol's idle timeout is
    /// typically hours, so what ends a parked reader is the guard's `shutdown`.
    pub read_timeout: Duration,
    /// The bound on writing one whole frame; see [`write_frame_deadline`].
    pub send_timeout: Duration,
    /// Bytes per blocking read. [`DEFAULT_READ_CHUNK`] unless the consumer's
    /// hosted reader uses a different one.
    pub chunk_size: usize,
    /// Depth of both the chunk and the frame channel.
    pub queue_depth: usize,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_secs(64_000),
            send_timeout: Duration::from_secs(30),
            chunk_size: DEFAULT_READ_CHUNK,
            queue_depth: 1,
        }
    }
}

/// Drive one already-connected socket with two blocking pumps, returning the
/// two owning adapters.
///
/// This is the whole seam for a caller that has no teardown thread: hand it a
/// connected `TcpStream`, receive an `AsyncRead` and an `AsyncWrite` that the
/// protocol code cannot tell from a split socket, with both pump threads owned
/// by the values returned.
///
/// The socket's blocking mode is taken over here (`own_blocking_mode`,
/// crate-private), and fatally: it is what makes both pumps' bounds hold by
/// construction rather than wherever a flag or option is honoured, so a target
/// that refused it would be a target this seam cannot bound, and that is worth
/// a failed dial rather than a silent park. `SO_RCVTIMEO` is still set, also
/// fatally, but as the *value* the reader's wait uses and as the mechanism on
/// the `not(unix)` arm; on unix the wait is a `POLLIN` poll. There is no
/// send-side counterpart: [`write_frame_deadline`] owns its own bound and needs
/// no socket option, so there is nothing here for a target that implements
/// fewer of them to switch off.
///
/// # The pool, and the two bands it carries
///
/// The two pumps come from one leased worker set of `pool`, taken atomically so
/// a circuit at capacity can never hold one pump and block for the other. Their
/// bands are the pool roster's, not this function's: at least one caller's
/// upstream C derives two — libca gives a circuit's receive thread
/// `highestPriorityLevelBelow(initializing thread)` and its send thread
/// `lowestPriorityLevelAbove(...)` (`tcpiiu.cpp:677-682`), so the sender sits
/// *above* the receiver and can always drain a queue the receiver's work is
/// filling — and a caller whose upstream uses one band for both (pvxs, one
/// reactor thread) declares the same band twice in its roster. The `Err` is now
/// admission's: [`io::ErrorKind::WouldBlock`] when the circuit pool is full,
/// otherwise a socket-option or thread-creation failure.
pub fn drive_socket_blocking(
    pool: &WorkerPool<2>,
    stream: TcpStream,
    label: &str,
    config: &PumpConfig,
) -> io::Result<(GuardedReader, GuardedWriter)> {
    let _ = stream.set_nodelay(true);
    // The mode both pumps' bounds are built on, fatal: see this function's docs.
    own_blocking_mode(&stream)?;
    // SO_RCVTIMEO, fatal: every target that runs this accepts it.
    stream.set_read_timeout(Some(config.read_timeout))?;
    // No SO_SNDTIMEO, on purpose. It was set here once, and it was the send
    // deadline's only way of regaining control — which made the deadline
    // conditional on an option VxWorks 7 does not implement (`ENOPROTOOPT`,
    // errno 42, measured on target). Setting it fatally aborted every CA client
    // circuit the instant its dial succeeded; setting it best-effort left the
    // writer pump able to park with nothing to reclaim it. Neither is a bound.
    // `write_frame_deadline` now waits for writability against its own
    // deadline, so the guarantee is the same on a target that implements every
    // socket option and on one that implements none.

    // Borrow the circuit's two pump workers as one set, or refuse. Roster order
    // is [reader, writer], the order `acquire` returns them.
    let (lease, [reader_worker, writer_worker]) = pool.acquire()?;
    let lease = Arc::new(lease);

    // One socket, two roles: the SAME descriptor shared through an `Arc`. See
    // the module docs for why this is not `try_clone`.
    let stream = Arc::new(stream);

    // The configured value directly, not read back off the socket: this is the
    // path that knows it, and it should not depend on the target implementing
    // the `SO_RCVTIMEO` *getter* as well as the setter.
    let (reader, reader_guard) = spawn_reader_pump_with_timeout(
        reader_worker,
        stream.clone(),
        label,
        config.chunk_size,
        config.queue_depth,
        Some(config.read_timeout),
    );
    let (writer, writer_guard) = spawn_writer_pump(
        writer_worker,
        stream,
        label,
        config.send_timeout,
        config.queue_depth,
    );

    Ok((
        GuardedReader {
            inner: reader,
            _guard: reader_guard,
            _lease: lease.clone(),
        },
        GuardedWriter {
            inner: writer,
            _guard: writer_guard,
            _lease: lease,
        },
    ))
}

/// The role roster a circuit's [`drive_socket_blocking`] pool must be built
/// with: `[reader, writer]`, both on `Small` stacks. The two bands are the
/// caller's to choose (see [`drive_socket_blocking`]'s docs).
pub fn circuit_roster(
    reader_priority: ThreadPriority,
    writer_priority: ThreadPriority,
) -> [WorkerRole; 2] {
    [
        WorkerRole {
            suffix: "reader",
            stack: StackSizeClass::Small,
            priority: reader_priority,
        },
        WorkerRole {
            suffix: "writer",
            stack: StackSizeClass::Small,
            priority: writer_priority,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::thread;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Production scope of this file: everything before the first column-0
    /// `#[cfg(test)]`.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The production scope with every comment removed.
    ///
    /// Both guards below forbid *code* from naming something, and this module's
    /// docs name several of those things at length precisely because explaining
    /// why they are forbidden is the point. Matching raw source made the
    /// `try_clone` guard fail on its own rationale — five prose hits, zero code
    /// hits — which is a guard that punishes documentation. Stripping comments
    /// first is what makes the assertion mean what it says.
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The RTEMS constraint this module exists to satisfy: it must not reach
    /// for tokio's async net/timer/spawn machinery, none of which builds for
    /// `armv7-rtems-eabihf`, and it must not suspend a future directly — every
    /// await goes through `block_on_sync`. `tokio::sync` and `tokio::io`'s
    /// traits ARE allowed and are what the two adapters are built from.
    ///
    /// Same guard the two blocking drivers carry, moved here with the code it
    /// describes. Needles are `concat!`-split so this body does not match
    /// itself under `include_str!`.
    #[test]
    fn the_blocking_io_seam_has_no_async_runtime_symbols() {
        let prod = code_only(production_scope(include_str!("blocking_io.rs")));
        // Fail closed: if the seam is no longer in the slice, the slice is
        // wrong and every assertion below would pass vacuously.
        assert!(
            prod.contains("fn drive_socket_blocking"),
            "production slice no longer covers the seam"
        );
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
                "the blocking I/O seam must not reference `{token}`: it has no async \
                 net/timer/spawn on RTEMS, and every await goes through `block_on_sync`"
            );
        }
    }

    /// The no-fd-dup rule, as a source-text guard rather than a comment.
    ///
    /// `try_clone` compiles everywhere and fails `ENXIO` on RTEMS only, so a
    /// reviewer who has not read the module docs has no local signal that it is
    /// wrong. This gives them one.
    #[test]
    fn the_seam_never_duplicates_a_descriptor() {
        let prod = code_only(production_scope(include_str!("blocking_io.rs")));
        assert!(
            prod.contains("fn drive_socket_blocking"),
            "production slice no longer covers the seam"
        );
        for token in [concat!("try", "_clone"), concat!("F_DUP", "FD")] {
            assert_eq!(
                prod.matches(token).count(),
                0,
                "`{token}` is back in the blocking I/O seam: on RTEMS 6 every fd \
                 duplication of a socket fails ENXIO. The read and write roles come \
                 from one descriptor shared through an `Arc`."
            );
        }
    }

    // ── adapter: cancel-safety ──────────────────────────────────────────

    /// Losing a `select!` race must consume nothing. A frame reader is used
    /// directly as a `select!` arm, so if this adapter dropped bytes on a lost
    /// race the failure would be silent and intermittent — a truncated frame
    /// long after the fact.
    ///
    /// Both boundaries of "what was in flight when the race was lost":
    ///
    /// * **mid-chunk** — part of a chunk has been handed out and the rest is
    ///   parked in `cur`/`pos`;
    /// * **pending** — no chunk has arrived at all, so the poll registered a
    ///   waker and returned `Pending`.
    #[epics_macros_rs::epics_test]
    async fn channel_reader_loses_no_bytes_when_a_select_race_is_lost() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
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
                // This arm always wins, so the read future below is created and
                // dropped without ever completing.
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

    /// A zero-length `poll_read` buffer must not eat a chunk.
    #[epics_macros_rs::epics_test]
    async fn a_zero_length_read_consumes_nothing() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
        let mut reader = ChannelReader::new(rx);
        tx.send(b"XY".to_vec()).await.expect("chunk queued");
        let mut none = [0u8; 0];
        assert_eq!(reader.read(&mut none).await.expect("empty read"), 0);
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).await.expect("real read");
        assert_eq!(&buf[..n], b"XY", "the chunk survived a zero-length read");
    }

    // ── adapter: the weak sender ────────────────────────────────────────

    /// The adapter must not be what keeps the frame channel open, or a pump
    /// would outlive the guard that is supposed to end it.
    #[epics_macros_rs::epics_test]
    async fn channel_writer_does_not_keep_the_frame_channel_open() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let room = Arc::new(WriteRoom::default());
        let mut writer = ChannelWriter {
            tx: tx.downgrade(),
            room,
        };
        writer.write_all(b"frame").await.expect("queued");
        assert_eq!(rx.recv().await.as_deref(), Some(&b"frame"[..]));

        // The guard's sender goes; the adapter is still alive and holding only
        // a weak handle.
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "a live ChannelWriter must not keep the channel open once the only \
             strong sender is gone"
        );
        assert!(
            writer.write_all(b"after").await.is_err(),
            "writing to a closed channel must be an error, not a silent drop"
        );
    }

    // ── the deadline loop ───────────────────────────────────────────────

    fn socket_pair() -> (StdTcpStream, StdTcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = StdTcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    /// A peer that never reads must not hold the writer pump past the deadline.
    ///
    /// unix-only: this asserts the POSIX loopback send-backpressure contract —
    /// a bounded send buffer, so a never-reading peer parks the sender.
    /// Windows grows the loopback send backlog dynamically and accepted the
    /// whole 8 MiB frame in 12 ms (measured, PR #56 CI 2026-07-24), so there
    /// is no backpressure for the deadline to trip there. The drivers that
    /// need the deadline run on `exec_backend`, which refuses Windows at
    /// compile time (`lib.rs`).
    #[cfg(unix)]
    #[test]
    fn the_deadline_loop_ends_a_trickling_peer() {
        let (client, server) = socket_pair();
        let send_timeout = Duration::from_millis(200);
        client
            .set_write_timeout(Some(send_timeout / 4))
            .expect("sndtimeo");
        // Never read from `server`, so the socket buffers fill and stay full.
        let big = vec![0u8; 8 * 1024 * 1024];
        let started = Instant::now();
        let err = write_frame_deadline(&client, &big, send_timeout)
            .expect_err("a peer that never reads must trip the deadline");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < send_timeout * 20,
            "the deadline bounded the whole frame, not each syscall: {:?}",
            started.elapsed()
        );
        drop(server);
    }

    /// The same bound, on a socket carrying **no `SO_SNDTIMEO` at all**.
    ///
    /// This is the VxWorks 7 boundary: `setsockopt(SO_SNDTIMEO)` is
    /// unimplemented there and returns `ENOPROTOOPT`, so no caller can arm the
    /// option however hard it tries
    /// (`doc/vxworks-circuit-wedge-on-target-measurement.md` §5). The two cases
    /// above cover "the option took"; this one covers "it did not", which is
    /// the only case where the deadline had nothing to regain control on.
    ///
    /// The write runs on its own thread and the assertion is on a bounded
    /// `recv`, because the failure being excluded is a park with no end: a
    /// direct call would hang the test rather than fail it.
    ///
    /// Runs on Darwin too, which is the point of it. `MSG_DONTWAIT` does not
    /// make an XNU send non-blocking, so while the flag was the only thing
    /// keeping the send out of a park this case was the one that failed there;
    /// it passes because [`own_blocking_mode`] no longer leaves the guarantee
    /// to the flag.
    #[cfg(unix)]
    #[test]
    fn the_deadline_holds_with_no_socket_send_timeout() {
        let (client, server) = socket_pair();
        // Deliberately no `set_write_timeout`. That is the whole boundary.
        let send_timeout = Duration::from_millis(200);
        // Never read from `server`, so the socket buffers fill and stay full.
        let big = vec![0u8; 8 * 1024 * 1024];
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        thread::spawn(move || {
            let outcome = write_frame_deadline(&client, &big, send_timeout).map_err(|e| e.kind());
            let _ = tx.send(outcome);
        });
        // Two orders of magnitude above the deadline, and still finite: what
        // this separates is "bounded" from "never".
        let outcome = rx
            .recv_timeout(send_timeout * 100)
            .expect("the frame's deadline must end the write without a socket timeout to lean on");
        assert_eq!(
            outcome.expect_err("a peer that never reads must trip the deadline"),
            io::ErrorKind::TimedOut
        );
        assert!(
            started.elapsed() < send_timeout * 20,
            "the deadline bounded the whole frame: {:?}",
            started.elapsed()
        );
        drop(server);
    }

    /// And the ordinary case still delivers.
    #[test]
    fn the_deadline_loop_delivers_a_frame_to_a_reading_peer() {
        let (client, mut server) = socket_pair();
        let send_timeout = Duration::from_secs(5);
        client
            .set_write_timeout(Some(send_timeout / 4))
            .expect("sndtimeo");
        let reader = thread::spawn(move || {
            let mut got = vec![0u8; 5];
            server.read_exact(&mut got).expect("read");
            got
        });
        write_frame_deadline(&client, b"hello", send_timeout).expect("delivered");
        assert_eq!(reader.join().expect("reader"), b"hello");
    }

    // ── guards ──────────────────────────────────────────────────────────

    /// A process-lifetime circuit pool for the pump/guard tests, so a `#[test]`
    /// can borrow a worker exactly as a real circuit does. Ample capacity so no
    /// test refuses; the tests that care about refusal build their own pool.
    static TEST_POOL: std::sync::LazyLock<WorkerPool<2>> = std::sync::LazyLock::new(|| {
        WorkerPool::new(
            "test",
            circuit_roster(ThreadPriority::Low, ThreadPriority::Low),
            64,
        )
    });

    /// Borrow one set: the lease and its reader/writer workers. The lease must
    /// be held until both pump jobs are joined, or the set would re-idle early.
    fn lease_pair() -> (SetLease, Worker, Worker) {
        let (lease, [reader, writer]) = TEST_POOL.acquire().expect("acquire a test set");
        (lease, reader, writer)
    }

    /// The reader guard's whole purpose: a pump parked in `read` behind a
    /// timeout longer than the test could wait is returned by the guard's drop.
    ///
    /// unix-only: this asserts the POSIX teardown contract — a local
    /// `shutdown(Shutdown::Both)` returns a thread parked in a blocking
    /// `read`. Windows does not provide that wake (measured, PR #56 CI
    /// 2026-07-24: the parked read outlived the 120 s test bound on x86_64),
    /// which is why `exec_backend` refuses Windows at compile time
    /// (`lib.rs`) — nothing there can reach the pumps' teardown.
    #[cfg(unix)]
    #[test]
    fn the_reader_guard_returns_a_pump_parked_in_read() {
        let (client, server) = socket_pair();
        // An effectively-infinite receive timeout: only the shutdown can end
        // this pump.
        client
            .set_read_timeout(Some(Duration::from_secs(64_000)))
            .expect("rcvtimeo");
        let (lease, reader_worker, _writer_worker) = lease_pair();
        let (reader, guard) = spawn_reader_pump(reader_worker, Arc::new(client), "parked", 4096, 1);
        // The peer sends nothing, so the pump is parked in `read`.
        let started = Instant::now();
        drop(reader);
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the guard's shutdown must return a parked read, not wait out SO_RCVTIMEO"
        );
        drop(lease);
        drop(server);
    }

    /// The writer guard must drop its sender *before* joining, or the join
    /// deadlocks against a pump parked on `recv()`.
    #[test]
    fn the_writer_guard_drops_its_sender_before_joining() {
        let (client, server) = socket_pair();
        let (lease, _reader_worker, writer_worker) = lease_pair();
        let (writer, guard) = spawn_writer_pump(
            writer_worker,
            Arc::new(client),
            "sender-order",
            Duration::from_secs(5),
            1,
        );
        let started = Instant::now();
        drop(writer);
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "dropping the guard must end a pump parked on recv(), not hang"
        );
        drop(lease);
        drop(server);
    }

    // ── loss announcements ──────────────────────────────────────────────

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

    /// The guards made a lost pump *survivable*; this is what makes it
    /// *visible*.
    ///
    /// `ReaderPumpGuard::drop` used to join and throw the result away, so a
    /// pump that unwound left nothing behind: the connection's own error is a
    /// bland channel-closed, and the two were unlinkable. Dropping must also not
    /// itself panic — a propagating drop would abort the process during another
    /// unwind.
    #[test]
    fn a_panicked_reader_pump_is_reported_and_not_discarded() {
        let (client, server) = socket_pair();
        let (lease, reader_worker, _writer_worker) = lease_pair();
        let lines = lines_while(|| {
            let _guard = ReaderPumpGuard {
                sock: Arc::new(client),
                label: "PVA connection 127.0.0.1:0".to_string(),
                job: Some(reader_worker.run(|| panic!("reader blew up"))),
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
        drop(lease);
        drop(server);
    }

    #[test]
    fn a_panicked_writer_pump_is_reported_and_not_discarded() {
        let (frames, _rx) = mpsc::channel::<Vec<u8>>(1);
        let (lease, _reader_worker, writer_worker) = lease_pair();
        let lines = lines_while(|| {
            let _guard = WriterPumpGuard {
                frames: Some(frames),
                label: "PVA connection 127.0.0.1:0".to_string(),
                job: Some(writer_worker.run(|| panic!("writer blew up"))),
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
        drop(lease);
    }

    /// The other boundary: an ordinary teardown is not a loss. Every connection
    /// that ever closes runs these drops, so announcing there would bury the
    /// real losses on a serial console.
    #[test]
    fn a_pump_that_ends_cleanly_is_not_announced() {
        let (client, server) = socket_pair();
        let (lease, reader_worker, _writer_worker) = lease_pair();
        let lines = lines_while(|| {
            let _guard = ReaderPumpGuard {
                sock: Arc::new(client),
                label: "PVA connection 127.0.0.1:0".to_string(),
                job: Some(reader_worker.run(|| {})),
            };
        });
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("was lost") || l.contains("panicked")),
            "an ordinary connection teardown must print nothing: {lines:?}"
        );
        drop(lease);
        drop(server);
    }

    /// Structural closure as source: a pump lost to a panic must be announced.
    /// Both guards report through the one announcement function; the old
    /// creation-failure loss is gone with the per-connection spawn — a pooled
    /// worker is borrowed, never created per connection.
    #[test]
    fn every_pump_loss_goes_through_the_announcement() {
        let prod = production_scope(include_str!("blocking_io.rs"));
        assert_eq!(
            code_only(prod)
                .matches(concat!("let _ = jo", "b.join()"))
                .count(),
            0,
            "a discarded join result is a panicked pump nobody hears about"
        );
        for owner in [
            "impl Drop for ReaderPumpGuard",
            "impl Drop for WriterPumpGuard",
        ] {
            let at = prod
                .find(owner)
                .unwrap_or_else(|| panic!("`{owner}` is gone from this module"));
            let body = &prod[at..(at + 900).min(prod.len())];
            assert!(
                body.contains(concat!("pump_thread_", "lost(")),
                "`{owner}` can lose a pump thread without saying so"
            );
        }
    }

    /// Both roles come from one descriptor, and both actually move bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_descriptor_serves_both_pumps() {
        let (client, mut server) = socket_pair();
        let (mut reader, mut writer) = drive_socket_blocking(
            &TEST_POOL,
            client,
            "127.0.0.1:0",
            &PumpConfig {
                read_timeout: Duration::from_secs(5),
                send_timeout: Duration::from_secs(5),
                ..PumpConfig::default()
            },
        )
        .expect("pumps started");

        let peer = thread::spawn(move || {
            let mut got = vec![0u8; 4];
            server.read_exact(&mut got).expect("peer read");
            server.write_all(b"pong").expect("peer write");
            got
        });

        writer.write_all(b"ping").await.expect("wrote");
        let mut got = [0u8; 4];
        reader.read_exact(&mut got).await.expect("read back");
        assert_eq!(&got, b"pong");
        assert_eq!(peer.join().expect("peer"), b"ping");
    }

    /// The invariant [`DialPool`] exists for: a dial *borrows* a thread, it does
    /// not create one.
    ///
    /// Sequential dials — the shape a reconnect loop makes — must all be served
    /// by the same worker, so the count of threads created over the process's
    /// life is 1 rather than one per attempt. The per-attempt shape this
    /// replaced would report 8 here (and leak 8 × 128 B of RTEMS TLS key).
    ///
    /// The tight spot is the *first* dial after a reply: the caller is woken by
    /// the very worker that must serve it next, so a pool that counted parked
    /// workers would see none available and create a second. That is why the
    /// assertion is inside the loop and not only after it.
    #[epics_macros_rs::epics_test]
    async fn sequential_dials_reuse_one_worker() {
        static POOL: DialPool = DialPool::new("test-dial", ThreadPriority::Low);
        const DIALS: usize = 8;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // Hold every accepted side open: a peer that closed would let a dial
        // fail for a reason this test is not about.
        let acceptor = thread::spawn(move || {
            (0..DIALS)
                .map(|_| listener.accept().expect("accept").0)
                .collect::<Vec<_>>()
        });

        for i in 0..DIALS {
            let dialed = POOL.dial(addr).expect("dial submitted");
            let stream = dialed
                .await
                .expect("the worker must reply")
                .expect("connect to a live listener");
            assert_eq!(
                POOL.worker_count(),
                1,
                "dial {i} created a new thread instead of reusing the idle \
                 worker: sequential dials must borrow one thread, not one each"
            );
            drop(stream);
        }

        assert_eq!(
            POOL.worker_count(),
            1,
            "{DIALS} sequential dials must have created exactly one thread"
        );
        drop(acceptor.join().expect("acceptor"));
    }
}
