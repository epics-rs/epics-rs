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
//!   per tick never trips it and holds the pump thread indefinitely. See
//!   [`write_frame_deadline`].
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

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON
// suite. Each drives an adapter, a real socket pump, or a `DialPool` worker;
// those threads are std threads that reach `block_on_sync`/`connect` with no
// runtime entered, which is the exec-backend path, so the tokio runtime here
// only hosts the assertions.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use tokio::io::ReadBuf;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::runtime::task::{StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread};

/// One blocking read, sized to match the frame readers that consume it so the
/// byte arrival pattern is the hosted one.
pub const DEFAULT_READ_CHUNK: usize = 4096;

/// How many `SO_SNDTIMEO` ticks fit inside one send deadline. The socket
/// timeout only exists to return control to the deadline loop; the deadline is
/// the real bound.
pub const SEND_TICKS_PER_DEADLINE: u32 = 4;

/// The `SO_SNDTIMEO` a caller should set on a socket whose writer pump runs
/// with `send_timeout`, so the deadline loop regains control several times
/// inside one deadline.
pub fn send_tick_for(send_timeout: Duration) -> Duration {
    (send_timeout / SEND_TICKS_PER_DEADLINE).max(Duration::from_millis(1))
}

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

/// How one pump thread names itself to the OS and to the operator.
///
/// Two strings rather than one because they answer different questions and are
/// read in different places: `thread_name` is what a target thread census or a
/// debugger shows, `label` is the subject of the `errlog` line an operator
/// reads when a pump is lost.
#[derive(Clone, Debug)]
pub struct PumpSpec {
    /// OS thread name, e.g. `"PVAS-reader 10.0.0.1:5075"`.
    pub thread_name: String,
    /// Operator-facing subject of any loss report, e.g.
    /// `"PVA connection 10.0.0.1:5075"`.
    pub label: String,
    /// EPICS priority band both pumps of a connection should share.
    pub priority: ThreadPriority,
}

/// Announce a pump thread that did not end normally.
///
/// The guards below make both losses *survivable* — the connection is torn
/// down however a pump ends. They do not make either loss *visible*, and a
/// process that has lost a thread but reads exactly like a healthy one is what
/// closes here. Two losses reach this function: the thread could not be created
/// at all, and the thread panicked.
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

/// Spawn one pump thread, announcing a failure to create it before the error
/// propagates.
///
/// `Small` stack for both: neither builds anything — the reader's whole frame
/// is a chunk buffer and a `read`/`send` loop, and the writer drains
/// already-encoded frames from a queue onto the socket. Whatever runs the
/// protocol state machine is the caller's thread, and is sized by the caller.
fn spawn_pump(
    role: &str,
    spec: &PumpSpec,
    body: impl FnOnce() + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    spawn_dedicated_thread(
        spec.thread_name.clone(),
        spec.priority,
        StackSizeClass::Small,
        body,
    )
    .inspect_err(|e| pump_thread_lost(role, &spec.label, &format!("could not be created ({e})")))
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
/// a SYN-blackholed peer holds one for the OS connect ladder (Linux
/// `tcp_syn_retries`, ~130 s) long after the awaiting side gave up at its own
/// bound. One worker would let a single unreachable peer head-of-line-block
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
/// `rtems-pva-ioc`): one unanswering name server starved every future on Medium
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

/// Blocking read loop. Ends on EOF, read error, or `SO_RCVTIMEO`; dropping `tx`
/// on the way out is the EOF signal to the adapter.
fn reader_pump(sock: Arc<TcpStream>, tx: mpsc::Sender<Vec<u8>>, chunk_size: usize, label: String) {
    // `impl Read for &TcpStream`: one shared descriptor, no `try_clone`.
    let mut sock = &*sock;
    let mut chunk = vec![0u8; chunk_size];
    loop {
        let n = match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
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
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ReaderPumpGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
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
            if handle.join().is_err() {
                pump_thread_lost("reader", &self.label, "panicked");
            }
        }
    }
}

/// Drive `sock`'s read half with a blocking thread, yielding the `AsyncRead`
/// half of the seam and the guard that retires it.
///
/// `queue_depth` is the chunk channel's depth. **One** is the faithful choice
/// for a demand-driven frame reader — one read per poll, each frame dispatched
/// fully before the next read — because it reproduces that with at most one
/// chunk of read-ahead, which the kernel receive buffer already provides. A
/// larger depth lets a fast peer queue chunks while a slow consumer blocks: a
/// behaviour change, not an optimisation.
pub fn spawn_reader_pump(
    sock: Arc<TcpStream>,
    spec: &PumpSpec,
    chunk_size: usize,
    queue_depth: usize,
) -> io::Result<(ChannelReader, ReaderPumpGuard)> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(queue_depth);
    let pump_sock = sock.clone();
    let label = spec.label.clone();
    let handle = spawn_pump("reader", spec, move || {
        reader_pump(pump_sock, tx, chunk_size, label)
    })?;
    Ok((
        ChannelReader::new(rx),
        ReaderPumpGuard {
            sock,
            label: spec.label.clone(),
            handle: Some(handle),
        },
    ))
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

/// Write one whole frame under a **deadline**, not merely a per-syscall
/// timeout.
///
/// A hosted writer bounds `write_all(&frame)` as a unit. Plain `SO_SNDTIMEO`
/// bounds each `write` syscall instead, so a peer that accepts one byte per
/// tick never trips it and holds the pump thread indefinitely — the exact
/// stuck-peer hazard the hosted timeout exists to prevent, on a resource (an OS
/// thread) that is scarcer on RTEMS than a task is on the host. The socket
/// timeout here is only what returns control to this loop; the deadline is the
/// bound.
///
/// A partial write on expiry needs no repair: the caller ends the pump and the
/// connection is torn down, so nothing is ever written to this socket again.
pub fn write_frame_deadline(
    sock: &TcpStream,
    frame: &[u8],
    send_timeout: Duration,
) -> io::Result<()> {
    // `impl Write for &TcpStream`: rebind so `write`/`flush` have a mutable
    // place to borrow, without needing `&mut TcpStream` from the caller.
    let mut sock = sock;
    let deadline = Instant::now() + send_timeout;
    let mut off = 0;
    while off < frame.len() {
        // Checked at the top so every way round the loop is bounded, including
        // an `Interrupted` storm.
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
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for WriterPumpGuard {
    fn drop(&mut self) {
        // Decisive because it is the only strong sender — [`ChannelWriter`]
        // holds a weak handle. The pump drains what is queued, sees `None`, and
        // exits; on its way out it shuts the socket.
        drop(self.frames.take());
        if let Some(handle) = self.handle.take() {
            // Same reading as [`ReaderPumpGuard`]'s: an `Err` is a panicked
            // pump, and a pump that unwound with frames still queued dropped
            // them.
            if handle.join().is_err() {
                pump_thread_lost("writer", &self.label, "panicked");
            }
        }
    }
}

/// Drive `sock`'s write half with a blocking thread, yielding the `AsyncWrite`
/// half of the seam and the guard that retires it.
///
/// `queue_depth` follows the same reasoning as [`spawn_reader_pump`]'s: a
/// producer that emits one frame at a time and waits for it gets, at depth 1,
/// the same backpressure a blocking socket write would.
///
/// The caller is responsible for setting `SO_SNDTIMEO` on the socket — see
/// [`send_tick_for`] — so the deadline loop regains control while a peer is
/// stalled.
pub fn spawn_writer_pump(
    sock: Arc<TcpStream>,
    spec: &PumpSpec,
    send_timeout: Duration,
    queue_depth: usize,
) -> io::Result<(ChannelWriter, WriterPumpGuard)> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(queue_depth);
    let room = Arc::new(WriteRoom::default());
    let adapter = ChannelWriter {
        tx: tx.downgrade(),
        room: room.clone(),
    };
    let label = spec.label.clone();
    let handle = spawn_pump("writer", spec, move || {
        writer_pump(sock, rx, room, send_timeout, label)
    })?;
    Ok((
        adapter,
        WriterPumpGuard {
            // The only strong sender moves into the guard, so it cannot be
            // dropped out of order with the join. `adapter` above already took
            // its weak handle.
            frames: Some(tx),
            label: spec.label.clone(),
            handle: Some(handle),
        },
    ))
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
/// Both socket timeouts are set here rather than left to the caller, because
/// getting `SO_SNDTIMEO` wrong silently disarms [`write_frame_deadline`]'s only
/// way of regaining control.
///
/// # Two priorities, not one
///
/// The two pumps take separate bands because at least one caller's upstream C
/// derives two: libca gives a circuit's receive thread
/// `highestPriorityLevelBelow(initializing thread)` and its send thread
/// `lowestPriorityLevelAbove(...)` (`tcpiiu.cpp:677-682`), so the sender sits
/// *above* the receiver and can always drain a queue the receiver's work is
/// filling. A caller whose upstream uses one band for both (pvxs, one reactor
/// thread) passes the same value twice, which states that sameness rather than
/// having the API assume it.
pub fn drive_socket_blocking(
    stream: TcpStream,
    spec_prefix: &str,
    label: &str,
    reader_priority: ThreadPriority,
    writer_priority: ThreadPriority,
    config: &PumpConfig,
) -> io::Result<(GuardedReader, GuardedWriter)> {
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(config.read_timeout))?;
    stream.set_write_timeout(Some(send_tick_for(config.send_timeout)))?;

    // One socket, two roles: the SAME descriptor shared through an `Arc`. See
    // the module docs for why this is not `try_clone`.
    let stream = Arc::new(stream);

    let reader_spec = PumpSpec {
        thread_name: format!("{spec_prefix}-reader {label}"),
        label: label.to_string(),
        priority: reader_priority,
    };
    let writer_spec = PumpSpec {
        thread_name: format!("{spec_prefix}-writer {label}"),
        label: label.to_string(),
        priority: writer_priority,
    };

    // The reader guard is bound first, so a writer-spawn failure below unwinds
    // through it rather than leaving a pump parked on a socket nobody holds.
    let (reader, reader_guard) = spawn_reader_pump(
        stream.clone(),
        &reader_spec,
        config.chunk_size,
        config.queue_depth,
    )?;
    let (writer, writer_guard) = spawn_writer_pump(
        stream,
        &writer_spec,
        config.send_timeout,
        config.queue_depth,
    )?;

    Ok((
        GuardedReader {
            inner: reader,
            _guard: reader_guard,
        },
        GuardedWriter {
            inner: writer,
            _guard: writer_guard,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream as StdTcpStream};
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[test]
    fn the_deadline_loop_ends_a_trickling_peer() {
        let (client, server) = socket_pair();
        let send_timeout = Duration::from_millis(200);
        client
            .set_write_timeout(Some(send_tick_for(send_timeout)))
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

    /// And the ordinary case still delivers.
    #[test]
    fn the_deadline_loop_delivers_a_frame_to_a_reading_peer() {
        let (client, mut server) = socket_pair();
        let send_timeout = Duration::from_secs(5);
        client
            .set_write_timeout(Some(send_tick_for(send_timeout)))
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

    fn test_spec(label: &str) -> PumpSpec {
        PumpSpec {
            thread_name: format!("test-pump {label}"),
            label: label.to_string(),
            priority: ThreadPriority::Low,
        }
    }

    /// The reader guard's whole purpose: a pump parked in `read` behind a
    /// timeout longer than the test could wait is returned by the guard's drop.
    #[test]
    fn the_reader_guard_returns_a_pump_parked_in_read() {
        let (client, server) = socket_pair();
        // An effectively-infinite receive timeout: only the shutdown can end
        // this pump.
        client
            .set_read_timeout(Some(Duration::from_secs(64_000)))
            .expect("rcvtimeo");
        let (reader, guard) =
            spawn_reader_pump(Arc::new(client), &test_spec("parked"), 4096, 1).expect("spawned");
        // The peer sends nothing, so the pump is parked in `read`.
        let started = Instant::now();
        drop(reader);
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the guard's shutdown must return a parked read, not wait out SO_RCVTIMEO"
        );
        drop(server);
    }

    /// The writer guard must drop its sender *before* joining, or the join
    /// deadlocks against a pump parked on `recv()`.
    #[test]
    fn the_writer_guard_drops_its_sender_before_joining() {
        let (client, server) = socket_pair();
        let (writer, guard) = spawn_writer_pump(
            Arc::new(client),
            &test_spec("sender-order"),
            Duration::from_secs(5),
            1,
        )
        .expect("spawned");
        let started = Instant::now();
        drop(writer);
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "dropping the guard must end a pump parked on recv(), not hang"
        );
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
        let lines = lines_while(|| {
            let _guard = ReaderPumpGuard {
                sock: Arc::new(client),
                label: "PVA connection 127.0.0.1:0".to_string(),
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
        drop(server);
    }

    #[test]
    fn a_panicked_writer_pump_is_reported_and_not_discarded() {
        let (frames, _rx) = mpsc::channel::<Vec<u8>>(1);
        let lines = lines_while(|| {
            let _guard = WriterPumpGuard {
                frames: Some(frames),
                label: "PVA connection 127.0.0.1:0".to_string(),
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

    /// The other boundary: an ordinary teardown is not a loss. Every connection
    /// that ever closes runs these drops, so announcing there would bury the
    /// real losses on a serial console.
    #[test]
    fn a_pump_that_ends_cleanly_is_not_announced() {
        let (client, server) = socket_pair();
        let lines = lines_while(|| {
            let _guard = ReaderPumpGuard {
                sock: Arc::new(client),
                label: "PVA connection 127.0.0.1:0".to_string(),
                handle: Some(thread::spawn(|| {})),
            };
        });
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("was lost") || l.contains("panicked")),
            "an ordinary connection teardown must print nothing: {lines:?}"
        );
        drop(server);
    }

    /// Structural closure as source, for the one loss no test can force: a
    /// thread that cannot be created. Both guards and the single spawn site must
    /// report through the one announcement function.
    #[test]
    fn every_pump_loss_goes_through_the_announcement() {
        let prod = production_scope(include_str!("blocking_io.rs"));
        assert_eq!(
            code_only(prod)
                .matches(concat!("let _ = han", "dle.join()"))
                .count(),
            0,
            "a discarded join result is a panicked pump nobody hears about"
        );
        for owner in [
            "impl Drop for ReaderPumpGuard",
            "impl Drop for WriterPumpGuard",
            "fn spawn_pump(",
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
            client,
            "TEST",
            "127.0.0.1:0",
            ThreadPriority::Low,
            ThreadPriority::Low,
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
    #[tokio::test]
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
