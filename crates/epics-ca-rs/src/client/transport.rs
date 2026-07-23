// RTEMS-EXEC-MODEL-ALLOW(9): checked - these run and pass in the feature-ON suite.
// Was 18: the nine that left are the three virtual-time watchdog modules
// (read_loop_tests, recv_watchdog_tests, write_loop_timeout_tests), now gated
// off under the feature because the circuit path's deadlines moved onto the
// runtime seam and `start_paused` cannot advance the seam's clock. Ratcheted
// DOWN, never up, without running the survivors under the feature.

use std::collections::HashMap;
use std::net::SocketAddr;
#[cfg(feature = "experimental-rust-tls")]
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
// The reactor-driven transport only: `tokio::net` does not compile for
// `armv7-rtems-eabihf`, and the blocking transport reaches `std::net` through
// `runtime::blocking_io` instead. Every use of this name is inside a
// `not(any(exec_backend, ca_blocking_client))` arm of the `dial_ca` seam.
#[cfg(not(any(exec_backend, ca_blocking_client)))]
use tokio::net::TcpStream;

use crate::channel::AccessRights;
use crate::protocol::*;

use super::types::{
    CircuitKey, DirectServerWriter, DirectServerWriters, InFlightOps, ReadReply, ReadReplyMode,
    ReadWaiter, SEND_BACKPRESSURE_FRAMES, TransportCommand, TransportEvent, WarmReplySlot,
};

fn dispatch_read_reply_with<F>(in_flight: &InFlightOps, ioid: u32, make_result: F)
where
    F: FnOnce(ReadReplyMode) -> epics_base_rs::error::CaResult<ReadReply>,
{
    // Hot path: warm waiter — peek the entry, take the Sender from its
    // slot, leave the entry in place so the next call on the same
    // channel can reuse this ioid without going through `alloc_ioid` +
    // DashMap insert/remove. Cold path: one-shot waiter, removed on
    // dispatch as before.
    //
    // Two DashMap touches on the cold path (1 read-lock `get` + 1
    // write-lock `remove`) instead of one — accepted because the
    // single-`get` cold path is network-bound (~70µs warm) and the
    // bulk-read hot path (this `Warm` branch) is what saves ~2µs/PV.
    let warm: Option<(ReadReplyMode, WarmReplySlot)> = {
        if let Some(entry) = in_flight.reads.get(&ioid) {
            match &*entry {
                ReadWaiter::Warm { mode, slot, .. } => Some((*mode, slot.clone())),
                ReadWaiter::OneShot { .. } => None,
            }
        } else {
            None
        }
    };
    if let Some((mode, slot)) = warm {
        let result = make_result(mode);
        if let Some(tx) = slot.lock().take() {
            let _ = tx.send(result);
        }
        return;
    }
    if let Some((_, waiter)) = in_flight.reads.remove(&ioid) {
        let result = make_result(waiter.mode());
        waiter.send(result);
    }
}

fn make_read_reply(
    mode: ReadReplyMode,
    data_type: u16,
    count: u32,
    data: &[u8],
) -> epics_base_rs::error::CaResult<ReadReply> {
    if matches!(mode, ReadReplyMode::Plain) && count == 1 {
        let dbr_type = DbFieldType::from_u16(data_type)?;
        let value = EpicsValue::from_bytes_array(dbr_type, data, count as usize)?;
        Ok(ReadReply::Plain { dbr_type, value })
    } else {
        Ok(ReadReply::Raw {
            data_type,
            count,
            data: data.to_vec(),
        })
    }
}

fn dispatch_read_error(in_flight: &InFlightOps, ioid: u32, error: epics_base_rs::error::CaError) {
    dispatch_read_reply_with(in_flight, ioid, |_| Err(error));
}

/// Optional client-side TLS handshaker. `None` means plaintext.
/// Behind the `tls` feature so default builds carry zero TLS code.
#[cfg(feature = "experimental-rust-tls")]
type TlsConnector = tokio_rustls::TlsConnector;
#[cfg(feature = "experimental-rust-tls")]
type ClientTlsConfig = Arc<tokio_rustls::rustls::ClientConfig>;

/// Timeout for echo response before declaring connection dead (matches C EPICS CA_ECHO_TIMEOUT).
const ECHO_TIMEOUT_SECS: u64 = 5;

/// C `tcpiiu::bytesArePendingInOS()` — "are there unread bytes still sitting
/// in the OS receive buffer right now?" (`tcpiiu.cpp:544`, an `ioctl(FIONREAD)`
/// under `osiSock`). This is the sole input to libca's flow control
/// (`tcpiiu.cpp:548-567`): it measures the *socket*, not any consumer-side
/// backlog, which is why libca can never latch `EVENTS_OFF` on a slow reader.
///
/// Boxed rather than generic so `read_loop` keeps one shape across the
/// plaintext, TLS and duplex-mock readers, each of which needs a different
/// occupancy source (or none, in tests).
type OsRecvQueueProbe = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// Occupancy probe over a live socket fd. Non-blocking `FIONREAD` through the
/// workspace's one owner of that ioctl
/// ([`epics_base_rs::runtime::blocking_io::pending_bytes`], which is also C
/// `rsrv`'s batch-up gate); a failed ioctl reports "nothing pending", which is
/// the safe answer — it can only clear flow control early, never latch it on.
///
/// Reached through the shared owner rather than a local `libc::ioctl` because
/// the constant is not the same everywhere: `libc` omits `FIONREAD` for
/// `armv7-rtems-eabihf` entirely, and the value newlib defines there had to be
/// derived by hand. A second copy of that derivation is a second thing to get
/// wrong.
#[cfg(unix)]
fn fd_recv_queue_probe(fd: std::os::fd::RawFd) -> OsRecvQueueProbe {
    /// `AsRawFd` over a borrowed descriptor this probe does not own. The fd
    /// belongs to the reader half held by the `read_loop` that holds this
    /// closure, so it stays open for the closure's life; wrapping it (rather
    /// than an `OwnedFd`) is what keeps that ownership where it is.
    struct BorrowedSocket(std::os::fd::RawFd);
    impl std::os::fd::AsRawFd for BorrowedSocket {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.0
        }
    }
    std::sync::Arc::new(move || {
        epics_base_rs::runtime::blocking_io::pending_bytes(&BorrowedSocket(fd)).is_ok_and(|n| n > 0)
    })
}

#[cfg(not(unix))]
fn fd_recv_queue_probe(_fd: std::os::raw::c_int) -> OsRecvQueueProbe {
    // No FIONREAD equivalent wired up here: report the socket as always
    // drained, which disables flow control rather than latching it on.
    std::sync::Arc::new(|| false)
}

/// Probe for tests whose subject is not flow control: the socket always reads
/// clean, so `read_loop` never asks for `EVENTS_OFF`.
#[cfg(test)]
fn drained_socket_probe() -> OsRecvQueueProbe {
    std::sync::Arc::new(|| false)
}

// ---------------------------------------------------------------------------
// The circuit's TCP dial — one seam, two transports
// ---------------------------------------------------------------------------

/// TCP keepalive on a virtual circuit: probe after this much idle time.
///
/// One number for both transports, so the *policy* has a single home even
/// though the syscall does not (`socket2` where it exists, raw `libc` where it
/// does not — see [`set_circuit_keepalive`]). The CA server's accept loop
/// applies the same ladder to the other end of the same wire
/// (`server/tcp.rs:1455-1464`).
const CIRCUIT_KEEPALIVE_IDLE: Duration = Duration::from_secs(15);

/// TCP keepalive on a virtual circuit: interval between probes once idle.
/// Three failures end the connection, so ~30 s to detect a half-open peer.
const CIRCUIT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// The EPICS priority a circuit's **receive** pump thread runs at.
///
/// C parity, derived rather than chosen: libca creates `CAC-TCP-recv` at
/// `cac::highestPriorityLevelBelow(initializing thread)` (`tcpiiu.cpp:677`),
/// and the thread that initializes the CA context for record links is C's
/// `dbCaLink` worker at `epicsThreadPriorityMedium`
/// (`dbCa.c:340`). On RTEMS `epicsThreadHighestPriorityLevelBelow` is exactly
/// `p - 1` (`RTEMS-score/osdThread.c:120-131`), so this is **49**.
#[cfg(any(exec_backend, ca_blocking_client))]
const CAC_RECV_PRIORITY: epics_base_rs::runtime::task::ThreadPriority =
    epics_base_rs::runtime::task::ThreadPriority::Custom(
        epics_base_rs::runtime::task::ThreadPriority::Medium.value() - 1,
    );

/// The EPICS priority a circuit's **send** pump thread runs at.
///
/// `cac::lowestPriorityLevelAbove(...)` (`tcpiiu.cpp:681`) — **51**, one band
/// *above* the receiver and above the link worker itself. The asymmetry is
/// libca's and is load-bearing: "the send thread runs at a higher priority
/// than the [receive thread]" (`tcpiiu.cpp:1716`), so a circuit whose receive
/// side is busy still drains its send queue promptly. Collapsing the two onto
/// one band would make a PUT wait behind an unrelated monitor burst.
#[cfg(any(exec_backend, ca_blocking_client))]
const CAC_SEND_PRIORITY: epics_base_rs::runtime::task::ThreadPriority =
    epics_base_rs::runtime::task::ThreadPriority::Custom(
        epics_base_rs::runtime::task::ThreadPriority::Medium.value() + 1,
    );

/// Every TCP dial this client makes, on a bounded set of permanent threads.
///
/// One pool for the process, at `CAC_RECV_PRIORITY` — the band C's own
/// `CAC-TCP-recv` connect runs on (`tcpiiu.cpp:677`), and the band of the
/// receive pump this dial precedes. The pool is per *band*, which is why the
/// PVA client cannot share it: its dials belong to `PVA_CLIENT_PRIORITY`.
///
/// `"CAC-dial"` and not `"CAC-connect {server_addr}"`: a reused worker cannot
/// be named for one circuit. A thread dump therefore still says *how many*
/// dials are stuck but no longer *which server* is not answering — the trade
/// for bounding the count, the same one the PVA pool makes. The server stays
/// in the `tracing::warn!` on every failure arm below.
#[cfg(any(exec_backend, ca_blocking_client))]
static CA_DIAL_POOL: epics_base_rs::runtime::blocking_io::DialPool =
    epics_base_rs::runtime::blocking_io::DialPool::new("CAC-dial", CAC_RECV_PRIORITY);

/// BRING-UP PROBE: dial attempts submitted to [`CA_DIAL_POOL`] since boot.
///
/// The denominator of the on-target bound measurement. `worker_count()` alone
/// cannot distinguish "bounded" from "never dialled twice", so the rig needs
/// the attempt count next to it — and on a target with no shell and no
/// reachable `caget` the only place to read either is the serial console.
#[cfg(all(feature = "bringup-probes", any(exec_backend, ca_blocking_client)))]
static CA_DIAL_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// BRING-UP PROBE: `(workers created, dial attempts submitted)` for the CA
/// client's dial pool.
///
/// Behind `bringup-probes` with the rest of the measurement rig
/// (`doc/calink-rtems-design.md` §11.7 item 3): a default image compiles
/// neither the counter nor this accessor.
#[cfg(all(feature = "bringup-probes", any(exec_backend, ca_blocking_client)))]
pub fn dial_pool_probe() -> (usize, usize) {
    (
        CA_DIAL_POOL.worker_count(),
        CA_DIAL_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Apply the circuit keepalive ladder to a connected socket.
///
/// `socket2` is the portability owner for this on every host: the option
/// names differ across unixes (`TCP_KEEPIDLE` on Linux and the BSDs,
/// `TCP_KEEPALIVE` on Darwin) and `libc` does not define the Darwin spelling
/// at all, so hand-rolling it here would trade one working call for a per-OS
/// table. See [`set_circuit_keepalive`]'s RTEMS sibling for the one target
/// where `socket2` is not in the dependency graph.
#[cfg(not(any(exec_backend, ca_blocking_client)))]
fn set_circuit_keepalive(stream: &TcpStream) {
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(CIRCUIT_KEEPALIVE_IDLE)
        .with_interval(CIRCUIT_KEEPALIVE_INTERVAL);
    let _ = sock.set_keepalive(true);
    let _ = sock.set_tcp_keepalive(&keepalive);
}

/// The same ladder through raw `libc`, for the transport `socket2` cannot
/// reach.
///
/// `socket2` is host-only in this crate's manifest (it does not build for
/// `armv7-rtems-eabihf`), so the blocking transport sets the three options
/// itself — the shape `server/blocking.rs:551` already uses for the server's
/// listening sockets. RTEMS's stack is FreeBSD's (`libbsd`), so the constants
/// are the BSD ones (`TCP_KEEPIDLE` = 256, `TCP_KEEPINTVL` = 512), supplied by
/// `libc` for the target.
///
/// Failures are ignored, exactly as on the `socket2` path: keepalive is a
/// backstop under CA's own echo watchdog (`ECHO_TIMEOUT_SECS`), not the
/// mechanism that detects a dead circuit.
#[cfg(all(unix, any(exec_backend, ca_blocking_client)))]
fn set_circuit_keepalive(stream: &std::net::TcpStream) {
    use std::os::fd::AsRawFd;

    fn set_opt(fd: std::os::fd::RawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) {
        // SAFETY: `fd` is a valid open socket owned by the caller for the
        // duration of the call; `value` outlives it; the size matches a
        // `c_int` option value.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    let fd = stream.as_raw_fd();
    set_opt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set_opt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPIDLE,
        CIRCUIT_KEEPALIVE_IDLE.as_secs() as libc::c_int,
    );
    set_opt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        CIRCUIT_KEEPALIVE_INTERVAL.as_secs() as libc::c_int,
    );
}

/// A circuit driven by two blocking pump threads instead of a reactor.
///
/// One value rather than a loose pair so the dial seam has one return type on
/// both transports, and so the TLS path — which needs a duplex, not two halves
/// — can wrap it exactly as it wraps a `TcpStream`.
#[cfg(any(exec_backend, ca_blocking_client))]
pub(super) struct PumpedCircuit {
    reader: epics_base_rs::runtime::blocking_io::GuardedReader,
    writer: epics_base_rs::runtime::blocking_io::GuardedWriter,
}

#[cfg(any(exec_backend, ca_blocking_client))]
impl AsyncRead for PumpedCircuit {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
    }
}

#[cfg(any(exec_backend, ca_blocking_client))]
impl AsyncWrite for PumpedCircuit {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// What [`dial_ca`] returns: a `tokio::net::TcpStream` on a hosted build, a
/// [`PumpedCircuit`] where there is no reactor to register one with.
#[cfg(not(any(exec_backend, ca_blocking_client)))]
pub(super) type CaCircuit = TcpStream;
/// See the hosted definition.
#[cfg(any(exec_backend, ca_blocking_client))]
pub(super) type CaCircuit = PumpedCircuit;

/// Split a dialled circuit into the two halves `read_loop` / `write_loop` take.
///
/// A free function rather than a method so the two transports' *different*
/// half types stay concrete — `read_loop` and `write_loop` are generic over
/// `AsyncRead`/`AsyncWrite`, so neither boxing nor a trait object is needed on
/// either side. The hosted arm keeps `into_split`'s owned halves (no `BiLock`,
/// unchanged from before this seam existed).
#[cfg(not(any(exec_backend, ca_blocking_client)))]
pub(super) fn split_circuit(
    stream: CaCircuit,
) -> (
    tokio::net::tcp::OwnedReadHalf,
    tokio::net::tcp::OwnedWriteHalf,
) {
    stream.into_split()
}

/// See the hosted definition.
#[cfg(any(exec_backend, ca_blocking_client))]
pub(super) fn split_circuit(
    stream: CaCircuit,
) -> (
    epics_base_rs::runtime::blocking_io::GuardedReader,
    epics_base_rs::runtime::blocking_io::GuardedWriter,
) {
    (stream.reader, stream.writer)
}

// ---------------------------------------------------------------------------
// The CA client's one TCP framing path
// ---------------------------------------------------------------------------

/// Why [`next_frame`] refuses the bytes at the head of a receive buffer.
///
/// Both variants mean the same thing to a caller — C
/// `tcpiiu.cpp::processIncoming:1197-1202` closes the circuit — and the
/// `Display` text is what each caller logs, so the two loops report the same
/// two failures in the same two words.
pub(super) enum FrameError {
    /// The header bytes are all present and the parser still rejected them.
    Header(epics_base_rs::error::CaError),
    /// `m_postsize & 0x7 != 0`. The wire spec requires an 8-byte-aligned
    /// payload; silently rounding up (an earlier `align8`) lets a hostile peer
    /// slide the framer into the middle of the next message.
    MisalignedPayload(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Header(e) => write!(f, "malformed TCP header ({e})"),
            Self::MisalignedPayload(n) => write!(f, "misaligned payload (postsize={n})"),
        }
    }
}

/// What the bytes at the head of a CA receive buffer are.
pub(super) enum Frame {
    /// A header was parsed. `hdr_size + body_len` is the message's total
    /// length on the wire — **the body may not have arrived yet**. What to do
    /// about that is the caller's business — but the size rule the caller
    /// applies is not per-circuit: both circuits run [`RecvBodyPolicy`], C's
    /// over-`EPICS_CA_MAX_ARRAY_BYTES` ignore-and-drain
    /// (`tcpiiu.cpp:1269-1283`), because C runs `processIncoming` on a
    /// name-service `tcpiiu` exactly as on a data one.
    Header {
        hdr: CaHeader,
        hdr_size: usize,
        body_len: usize,
    },
    /// Not enough bytes to decide yet — read more, keep what is here.
    Incomplete,
    /// Definitively malformed. C closes the circuit; so do both callers.
    Malformed(FrameError),
}

/// **The CA client's one TCP framing step**, shared by the upstream circuit's
/// [`read_loop`] and the `EPICS_CA_NAME_SERVERS` circuit's reader
/// (`client/search.rs::run_nameserver_connection`).
///
/// The name-service circuit used to carry its own ~100-line copy of these
/// rules, which is the shape `doc/calink-rtems-design.md` §6 C2 named: "one
/// seam, two callers, not two seams and three framing loops". The dial became
/// one seam in `aa91860b` ([`dial_ca`]); this is the framing half. The two
/// copies had already drifted apart once — the misaligned-postsize close and
/// the partial-extended-header wait were fixed twice, separately — which is
/// the cost this function removes.
///
/// It answers **only** the header-level question, deliberately: how many bytes
/// this message occupies and whether the peer is still speaking CA. Body
/// policy (the receive-side size limit, the drain-across-reads) stays with the
/// loop that owns the receive buffer, because that policy is genuinely
/// per-circuit — see [`Frame::Header`].
pub(super) fn next_frame(buf: &[u8]) -> Frame {
    if buf.len() < CaHeader::SIZE {
        return Frame::Incomplete;
    }
    // C `tcpiiu.cpp::processIncoming` distinguishes a *partial* extended
    // header (await more bytes) from a *definitively malformed* one (close).
    // Extended form is `m_postsize == 0xffff` alone (`tcpiiu.cpp:1168`), and
    // it demands 24 header bytes rather than 16; fewer than that present is
    // the one legitimate "await more" case beyond a short base header.
    let base_post = u16::from_be_bytes([buf[2], buf[3]]);
    if base_post == 0xFFFF && buf.len() < CaHeader::SIZE + 8 {
        return Frame::Incomplete;
    }
    let (hdr, hdr_size) = match CaHeader::from_bytes_extended(buf) {
        Ok(v) => v,
        // `from_bytes_extended` rejects exactly two inputs — under 16 bytes,
        // and `0xFFFF` with under 24 — and both are excluded above, so this
        // arm is unreachable today. It is C's close rather than an
        // `unreachable!` on purpose: a parser that later grows a rejection
        // must reach the peer-is-malformed path, not panic the circuit's task.
        Err(e) => return Frame::Malformed(FrameError::Header(e)),
    };
    let body_len = hdr.actual_postsize();
    if body_len & 0x7 != 0 {
        return Frame::Malformed(FrameError::MisalignedPayload(body_len));
    }
    Frame::Header {
        hdr,
        hdr_size,
        body_len,
    }
}

/// **The CA client's one receive-side body limit**, shared by the upstream
/// circuit's [`read_loop`] and the `EPICS_CA_NAME_SERVERS` circuit's reader
/// (`client/search.rs::run_nameserver_connection`) — the body-policy half of
/// the seam whose framing half is [`next_frame`].
///
/// C `tcpiiu::processIncoming`'s limit and its ignore-don't-close policy
/// (`tcpiiu.cpp:1207-1283`) apply to *every* `tcpiiu`, and a name-service
/// circuit is a `tcpiiu` (`isNameService()`), so C runs one rule on both.
/// `None` — the C default (`EPICS_CA_AUTO_ARRAY_BYTES=YES`) — means the
/// circuit accepts any payload the server announces; an operator who turns it
/// off gets a limit, and over-limit responses are dropped one-by-one with a
/// single log line per circuit. Neither case ever closes the circuit.
///
/// Pre-fix the name-service reader carried no limit at all: with
/// `EPICS_CA_AUTO_ARRAY_BYTES=NO` the data circuit refused and drained an
/// over-limit body while the name-server circuit buffered whatever a header
/// announced — up to 4 GiB from one 24-byte extended header — which is
/// exactly the "two copies drift" failure the framing seam exists to prevent.
pub(super) struct RecvBodyPolicy {
    limit: Option<usize>,
    /// C `tcpiiu.cpp:1276-1282` drains an ignored oversize body across reads
    /// with `recvQue.removeBytes` and returns to await the rest. This is that
    /// counter: while it is non-zero every byte received belongs to a message
    /// already refused, so it is consumed before framing resumes.
    bytes_to_drain: usize,
    oversize_logged: bool,
}

impl RecvBodyPolicy {
    pub(super) fn new() -> Self {
        Self::with_limit(crate::protocol::max_recv_body_bytes())
    }

    /// Test seam: the boundary table below injects a limit instead of
    /// mutating the process environment under a parallel test runner.
    fn with_limit(limit: Option<usize>) -> Self {
        Self {
            limit,
            bytes_to_drain: 0,
            oversize_logged: false,
        }
    }

    /// First step of every receive iteration: consume bytes still owed to an
    /// already-refused message from the head of `accumulated`. Returns `true`
    /// while the refused message has bytes outstanding beyond this read — the
    /// caller reads again instead of framing.
    pub(super) fn drain_refused(&mut self, accumulated: &mut Vec<u8>) -> bool {
        if self.bytes_to_drain > 0 {
            let take = self.bytes_to_drain.min(accumulated.len());
            accumulated.drain(..take);
            self.bytes_to_drain -= take;
        }
        self.bytes_to_drain > 0
    }

    /// Whether a message announcing `body_len` payload bytes is refused under
    /// the operator's limit. The first refusal on a circuit logs C's one line
    /// (`tcpiiu.cpp:1271`); the rest are silent so a misbehaving server
    /// cannot flood the log.
    pub(super) fn refuses(&mut self, peer: SocketAddr, body_len: usize) -> bool {
        let over = self.limit.is_some_and(|limit| body_len > limit);
        if over && !self.oversize_logged {
            eprintln!(
                "CA: {peer}: response with payload size={body_len} \
                 > EPICS_CA_MAX_ARRAY_BYTES ignored"
            );
            self.oversize_logged = true;
        }
        over
    }

    /// Register the still-arriving tail of a refused message, to be consumed
    /// by [`Self::drain_refused`] on subsequent reads.
    pub(super) fn owe(&mut self, bytes: usize) {
        self.bytes_to_drain = bytes;
    }
}

/// **The CA client's one TCP dial**, and the one place a circuit's socket
/// options are set.
///
/// Two implementations, selected at compile time and never at runtime:
///
/// * `tokio_backend` — `tokio::net::TcpStream`, the reactor-driven socket this
///   client has always used;
/// * `exec_backend` or `--cfg ca_blocking_client` —
///   `runtime::blocking_io`'s two pump threads over one `Arc<TcpStream>`
///   (never `try_clone`: `F_DUPFD` fails `ENXIO` on any libbsd socket,
///   measured on target).
///
/// The arm is chosen by the *backend*, not by the target. `exec_backend` is
/// `target_os = "rtems"` **or** `--features rtems-exec-model` (`build.rs`), and
/// what both share is that a future started through `runtime::task::spawn`
/// runs with no tokio reactor entered — on RTEMS because there is none and
/// `tokio::net` does not compile for the triple, on a host exec-model build
/// because the future lands on a callback-pool worker the runtime was never
/// entered on. A `tokio::net::TcpStream::connect` there panics ("there is no
/// reactor running") even though the process has a runtime elsewhere. Gating
/// this seam on `target_os = "rtems"` named the target where the fact it needs
/// is the backend, which is why `rtems-ca-ioc` still panicked on its first
/// dial after the UDP seam was fixed (`doc/calink-rtems-design.md` §10.10
/// item 2).
///
/// Both return the OS receive-queue probe alongside the circuit, because the
/// fd it reads has to be captured *before* the socket is split or wrapped —
/// making that one step is what stops a caller from forgetting it and
/// silently disabling libca's flow control (`fd_recv_queue_probe`).
///
/// `None` on failure, with the reason logged: the search engine retries the
/// address on its own cadence, which is what C's `disconnectNotify` + `break`
/// leaves to the same layer.
pub(super) async fn dial_ca(server_addr: SocketAddr) -> Option<(CaCircuit, OsRecvQueueProbe)> {
    #[cfg(not(any(exec_backend, ca_blocking_client)))]
    {
        // No application-level connect deadline. C `tcpRecvThread::connect`
        // (`tcpiiu.cpp:606-661`) issues a *blocking* `::connect()` and lets the
        // OS TCP stack bound it — on Linux that is tcp_syn_retries, ~130 s of
        // exponentially backed-off SYNs. A hardcoded 5 s cap here made every
        // server whose handshake takes longer than that (SYN-lossy path,
        // congested WAN link) permanently unreachable from the port while a C
        // client on the same wire connects fine.
        let stream = match TcpStream::connect(server_addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(server = %server_addr, error = %e, "TCP connect failed");
                return None;
            }
        };
        let _ = stream.set_nodelay(true);
        set_circuit_keepalive(&stream);
        let probe = recv_queue_probe_for(&stream);
        Some((stream, probe))
    }
    #[cfg(any(exec_backend, ca_blocking_client))]
    {
        dial_blocking(server_addr).await
    }
}

/// The blocking dial: connect on a thread of its own, set the options, start
/// the two pumps.
///
/// # Why the connect gets a thread
///
/// It is a *blocking* `::connect()` with **no application-level deadline** —
/// C's exact shape (`tcpiiu.cpp:606-661`), and the property
/// `connect_deadline_tests::tcp_connect_has_no_application_level_deadline`
/// pins for both transports. On Linux the OS bound is `tcp_syn_retries`,
/// ~130 s of exponentially backed-off SYNs.
///
/// Running that on the calling task's worker would park it for the whole
/// ladder. On this target the caller is a cooperative-executor worker shared
/// with every other future on its callback band — record-processing tails
/// included — so one unreachable server would stall the band for two minutes.
/// C does not have that problem because its connect is already on the
/// circuit's own `CAC-TCP-recv` thread (`tcpiiu.cpp:677`), created before the
/// connect rather than after it.
///
/// So the connect takes a thread here too, at the same band C's receive thread
/// takes — but a *borrowed* one, not a created one.
///
/// # Why the thread is borrowed and not created
///
/// The first cut created the thread per attempt and let it exit, which reads
/// as strictly cheaper than C's permanent per-circuit `CAC-TCP-recv`: fewer
/// live threads, one `std::thread` TLS-key allocation (128 B on RTEMS,
/// measured) more per attempt. That accounting is wrong in the term that
/// matters. On RTEMS the 128 B is *never returned* — the TLS key is freed
/// before the key's destructor runs — so the cost is per thread **creation**,
/// and C's permanent thread pays it exactly once per circuit while a
/// per-attempt thread pays it once per *attempt*. `run_nameserver_connection`
/// (`client/search.rs`) redials a failed address every `EPICS_CA_CONN_TMO`
/// (30 s) indefinitely, which is C's own cadence and is not going to change,
/// so a name server that is down leaked without a ceiling for as long as the
/// IOC ran.
///
/// The dial thread therefore comes from [`CA_DIAL_POOL`] — at most
/// `MAX_DIAL_WORKERS` permanent workers for the whole process — and is
/// returned to it when the connect resolves. The bound is by construction:
/// past the first dial at each concurrency level there is nothing left to
/// create, whatever the redial cadence. See `runtime::blocking_io::DialPool`,
/// and `doc/pvalink-rtems-design.md` §9.11 for the PVA half of the same fix.
///
/// A worker is still the socket's single finalizer: a receiver dropped by an
/// aborted caller only makes the send fail, and the fresh socket is dropped —
/// and closed — right there, before the worker takes its next request.
///
/// # What the pool does *not* do
///
/// It adds no deadline. R7-19 (`connect_deadline_tests`) is unchanged: the
/// worker issues a plain blocking `::connect()` and the awaiting side below
/// applies no bound of its own, so the OS remains the only thing that ends a
/// dial — C's shape (`tcpiiu.cpp:606-661`). The consequence, stated rather
/// than papered over: a worker pinned against a SYN-blackholed server is held
/// for the OS ladder (~130 s), and with every worker so pinned a further dial
/// waits in the queue instead of connecting at once. That is a latency
/// regression only in a case the contract already permits 130 s for, and it
/// takes `MAX_DIAL_WORKERS` *distinct* blackholed servers dialed at once to
/// reach — a circuit is dialed once per address at a time
/// (`connect_server`'s map lookup), so the re-offer loop cannot produce it by
/// retrying one address.
#[cfg(any(exec_backend, ca_blocking_client))]
async fn dial_blocking(server_addr: SocketAddr) -> Option<(CaCircuit, OsRecvQueueProbe)> {
    use epics_base_rs::runtime::blocking_io::{PumpConfig, drive_socket_blocking};

    #[cfg(feature = "bringup-probes")]
    CA_DIAL_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dialed_rx = match CA_DIAL_POOL.dial(server_addr) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!(server = %server_addr, error = %e, "cannot start the circuit dial thread");
            return None;
        }
    };
    let stream = match dialed_rx.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(server = %server_addr, error = %e, "TCP connect failed");
            return None;
        }
        Err(_) => {
            // The thread ended without sending: it panicked. Nothing to
            // retry here — the search engine re-offers the address.
            tracing::warn!(server = %server_addr, "circuit dial thread ended without a result");
            return None;
        }
    };
    set_circuit_keepalive(&stream);
    let probe = recv_queue_probe_for(&stream);
    // `read_timeout` keeps `PumpConfig`'s effectively-infinite default. The
    // reader pump ends the connection when its `SO_RCVTIMEO` expires, so any
    // finite value here is an idle-disconnect bound — and an idle CA circuit is
    // *supposed* to be silent: `read_loop` sends CA_PROTO_ECHO after
    // `EPICS_CA_CONN_TMO` of quiet and only its echo watchdog
    // (`ECHO_TIMEOUT_SECS`) is entitled to call the circuit dead. `send_timeout`
    // is the bound on writing one whole frame, which is the same number
    // `write_loop` already applies to its own send.
    let (reader, writer) = match drive_socket_blocking(
        stream,
        "CAC",
        &server_addr.to_string(),
        CAC_RECV_PRIORITY,
        CAC_SEND_PRIORITY,
        &PumpConfig {
            send_timeout: connection_timeout(),
            ..PumpConfig::default()
        },
    ) {
        Ok(halves) => halves,
        Err(e) => {
            tracing::warn!(server = %server_addr, error = %e, "circuit pump threads failed to start");
            return None;
        }
    };
    Some((PumpedCircuit { reader, writer }, probe))
}

/// Capture the OS receive-queue probe for a socket that is about to be split.
///
/// C `tcpiiu::bytesArePendingInOS()` is an ioctl on the circuit's socket. The
/// fd has to be taken while the whole socket is still in hand — the reader
/// half handed to `read_loop` keeps it open for exactly as long as the probe
/// can be called.
#[cfg(unix)]
fn recv_queue_probe_for<S: std::os::fd::AsRawFd>(sock: &S) -> OsRecvQueueProbe {
    fd_recv_queue_probe(sock.as_raw_fd())
}

/// See the unix definition; no `FIONREAD` equivalent is wired up elsewhere.
#[cfg(not(unix))]
fn recv_queue_probe_for<S>(_sock: &S) -> OsRecvQueueProbe {
    fd_recv_queue_probe(0)
}

/// `EPICS_CA_CONN_TMO` — C's `cac::connectionTimeout()`, default 30 s.
///
/// One knob, two uses, exactly as in C: it is the idle interval after
/// which a circuit sends CA_PROTO_ECHO, and it is the retry cadence a
/// name-service circuit waits out after a failed connect
/// (`tcpiiu.cpp:653-657`). Not a connect *deadline* — C's `::connect()`
/// blocks under the OS timeout, and nothing caps it.
///
/// C `cac.cpp:186-194` parses CONN_TMO as `double` and falls
/// back to the default (30 s) on parse failure, on `<= 0.0`, AND on
/// any value libca's bookkeeping treats as a sentinel for "use the
/// default". Pre-fix Rust used `.max(1.0) as u64` which (a) rounded
/// any positive sub-second value up to 1 s (`0.5` → 1) instead of
/// honouring it verbatim, (b) truncated fractional seconds via
/// `as u64` (`15.9` → 15), and (c) clamped explicit `0` to 1 s
/// instead of falling back to the default. Match C: keep as
/// `Duration` with full sub-second precision; only `parse error`
/// or `value <= 0.0` falls back to the default.
///
/// Parsing is C's `envGetDoubleConfigParam` → `epicsScanDouble`
/// (`crate::estdlib`), so `0x10` is 16 s and `1e400` is an ERANGE
/// failure, and the conversion is the saturating one — an explicit
/// `inf` is C's never-expiring deadline, not a panic.
///
/// Resolved ONCE per process, as C resolves it once in the `cac`
/// constructor and stores it in `cac::connTMO`: re-reading `getenv` on
/// every circuit would let the value drift mid-run and would repeat the
/// diagnostic below on every reconnect. [`prime_connection_timeout`] does
/// the resolution at client construction, where C does it.
pub(crate) fn connection_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(resolve_connection_timeout)
}

/// Resolve `EPICS_CA_CONN_TMO` now, so its diagnostic lands at context
/// creation (C `cac.cpp:188-194`) rather than on the first circuit.
pub(crate) fn prime_connection_timeout() {
    let _ = connection_timeout();
}

/// The uncached resolution behind [`connection_timeout`].
fn resolve_connection_timeout() -> Duration {
    use epics_base_rs::runtime::env_table::EPICS_CA_CONN_TMO;
    // C `CA_CONN_VERIFY_PERIOD` (`cac.cpp:190`) is a hand-copy of the table's
    // "30.0"; here the number has one home.
    let default_secs: f64 = EPICS_CA_CONN_TMO
        .default_str()
        .parse()
        .expect("EPICS_CA_CONN_TMO's compiled default is a number");
    // Unset resolves to the compiled default string and parses, silently — so
    // only a set-but-bad value reaches the error arm.
    let secs = match EPICS_CA_CONN_TMO.double() {
        Ok(v) => v,
        // C `cac::cac` (`cac.cpp:189-194`) — both lines, verbatim, on top
        // of the "Unable to find a real number in ..." that
        // `envGetDoubleConfigParam` already printed.
        Err(_) => {
            eprintln!("EPICS \"EPICS_CA_CONN_TMO\" double fetch failed");
            eprintln!("Defaulting \"EPICS_CA_CONN_TMO\" = {default_secs:.6}");
            default_secs
        }
    };
    // DOCUMENTED DEVIATION — a non-positive (or NaN) period does not become a
    // zero-period watchdog here.
    //
    // C stores whatever `envGetDoubleConfigParam` parsed (`cac.cpp:188-194`:
    // the default is applied ONLY when the fetch fails, never when it succeeds
    // with a useless number) and hands it to the circuit's connection-verify
    // watchdog. With `EPICS_CA_CONN_TMO=-5` or `=0` the deadline is already in
    // the past on every check, so the compiled `camonitor` — still delivering
    // updates, so the client is not "broken" — emitted 177_182 stderr lines in
    // 3 seconds on this platform, a "Virtual circuit unresponsive" flood
    // spinning a core. That is a degenerate behaviour with no operational
    // meaning, and reproducing it faithfully would hand any operator with a
    // typo'd env var a livelocked client.
    //
    // So the non-positive value is refused rather than obeyed, and — unlike
    // the silent guard this replaces — the operator is told that the value
    // they set is not the one in force. C prints nothing here because C obeys
    // it; this line is the port's, not libca's.
    if secs > 0.0 {
        crate::estdlib::duration_from_secs(secs)
    } else {
        eprintln!(
            "Warning: \"EPICS_CA_CONN_TMO\" = {secs} is not a positive period; \
             using {default_secs:.6} (a non-positive period fires the connection \
             watchdog continuously)"
        );
        crate::estdlib::duration_from_secs(default_secs)
    }
}
/// Legacy seconds accessor kept for call sites that need a coarse
/// number (e.g. `tokio::time::sleep(Duration::from_secs(N))` over a
/// long interval where sub-second precision does not matter). New
/// timer code should call `connection_timeout()` directly.
fn echo_idle_secs() -> u64 {
    let d = connection_timeout();
    d.as_secs().max(1)
}

/// Cap on the client-side TLS handshake, `EPICS_CA_TLS_HANDSHAKE_TMO`
/// (port-specific; libca has no TLS). Floored at 1 s, default 10 s.
///
/// Resolved once per process, like every other env-derived duration here.
#[cfg(feature = "experimental-rust-tls")]
fn tls_handshake_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        crate::estdlib::env_double("EPICS_CA_TLS_HANDSHAKE_TMO")
            .ok()
            .map(|v| crate::estdlib::duration_from_secs(v.max(1.0)))
            .unwrap_or(Duration::from_secs(10))
    })
}

struct ServerConnection {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Peer's minor protocol version, published by `read_loop` when the
    /// circuit's `CA_PROTO_VERSION` frame arrives. Request framing needs
    /// it: libca hands `CA_V49 ( minorProtocolVersion )` to
    /// `comQueSend::insertRequestHeader` as `v49Ok` and refuses the
    /// extended (24-byte) header for older peers (`comQueSend.cpp:285-363`).
    /// 0 (pre-V49) until the VERSION frame is seen, matching C's `tcpiiu`.
    server_minor: std::sync::Arc<std::sync::atomic::AtomicU16>,
    /// Beacon-arrival channel into `read_loop`. `false` = healthy
    /// beacon (refresh idle watchdog deadline); `true` = anomaly
    /// classified by `beacon_monitor` (set the in-loop flag so
    /// subsequent healthy beacons don't refresh the deadline either,
    /// causing the watchdog to expire on schedule and probe the
    /// circuit then). Mirrors libca's `tcpRecvWatchdog` model — see
    /// `TransportCommand::BeaconArrivalNotify` for full rationale.
    #[cfg(ca_beacon_monitor)]
    beacon_arrival_tx: mpsc::UnboundedSender<bool>,
    // Spawned via `runtime::task::spawn`, so typed as the seam handle.
    // Byte-identical to `tokio::task::JoinHandle` under the hosted default;
    // the executor's `JoinFuture` under `rtems-exec-model`.
    _read_task: epics_base_rs::runtime::task::TaskHandle<()>,
    _write_task: epics_base_rs::runtime::task::TaskHandle<()>,
}

/// Hard-stop on drop: abort both the per-server read and write tasks.
/// Without this, every code path that drops a `ServerConnection` (the
/// `connections.remove` on send-buffer stall in `send_frame`, the
/// implicit HashMap drop when `run_transport_manager` returns or its
/// task is aborted) would detach the inner JoinHandles, leaving the
/// per-server read/write tasks running until process exit. The
/// `read_task` holds a clone of `write_tx` and the `pending_frames`
/// Arc, so detaching it keeps the writer alive too. The companion
/// `CaClient::Drop` only aborts the four top-level tasks
/// (`coordinator` / `search` / `transport` / `beacon`); without this
/// `impl Drop`, aborting the transport manager would not cascade to
/// the per-circuit tasks it owns.
impl Drop for ServerConnection {
    fn drop(&mut self) {
        self._read_task.abort();
        self._write_task.abort();
    }
}

/// Per-task transport manager.
///
/// `in_flight` is the Option-C Phase-A shared in-flight read/write
/// registry: each spawned per-server `read_loop` gets a clone so it
/// can dispatch `READ_NOTIFY` / `WRITE_NOTIFY` responses straight to
/// the originating caller's oneshot, without a coordinator hop.
///
/// `last_rx_at` is the per-server "last frame received" sidecar
/// (Option C, Phase D): the read loop bumps it on every TCP frame
/// so `ca_receive_watchdog_delay` stays accurate even for read-only
/// or write-only workloads whose responses no longer reach the
/// coordinator.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_transport_manager(
    mut command_rx: mpsc::UnboundedReceiver<TransportCommand>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    server_writers: DirectServerWriters,
    last_rx_at: super::types::ServerLastRxAt,
    // Shared client identity (user / host). Cloned per connect so each
    // new circuit handshakes with the value current at connect time.
    client_identity: super::types::ClientIdentitySlot,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<ClientTlsConfig>,
    #[cfg(feature = "experimental-rust-tls")] tls_server_name: Option<String>,
    // Per-server SNI / cert-verification overrides built from the
    // hostname half of EPICS_CA_NAME_SERVERS=host:port entries.
    // Looked up per connect_server call so each TLS handshake uses
    // the operator-supplied DNS name for that specific peer; falls
    // back to tls_server_name (the global override), then the IP
    // literal. Empty when no name servers were given by hostname.
    #[cfg(feature = "experimental-rust-tls")] sni_overrides: HashMap<SocketAddr, String>,
) {
    // circuits are keyed by `(SocketAddr, priority)`, so two
    // channels to the same IOC at different priorities own independent
    // TCP circuits (libca `caServerID`).
    let mut connections: HashMap<CircuitKey, ServerConnection> = HashMap::new();
    // Pending connect_server tasks. Spawning each connect into a
    // task set (rather than `.await`-ing inline) is what lets a
    // slow TCP/TLS handshake on circuit A stop blocking unrelated
    // commands: BeaconArrivalNotify for already-connected
    // circuits, fast-path CreateChannel for circuit B, etc. The
    // task returns its `CircuitKey` alongside the result so
    // `join_next` can pair completion with the right state.
    //
    // `runtime::task::TaskSet`, not `tokio::task::JoinSet`: this loop runs on
    // a callback band on the RTEMS target, where `JoinSet::spawn` — which is
    // `tokio::spawn` under another name — panics with *"there is no reactor
    // running"* at the first connect and takes the band worker with it
    // (measured, `doc/calink-rtems-design.md` §11.1). The seam type keeps
    // the concurrency, the completion pairing and the abort-on-drop.
    let mut pending_connects: epics_base_rs::runtime::task::TaskSet<(
        CircuitKey,
        Option<ServerConnection>,
    )> = epics_base_rs::runtime::task::TaskSet::new();
    // Commands waiting on a pending connect. Keyed by the command's
    // target circuit. CreateChannel is the only command that *causes*
    // a connect to start; subsequent CreateChannels for the same
    // circuit (and any non-CreateChannel commands that happen to
    // arrive before connect completes) all queue here and get drained
    // when the connect resolves.
    let mut queued_per_server: HashMap<CircuitKey, Vec<TransportCommand>> = HashMap::new();

    // Helper: resolve the right SNI / cert-verification name for a
    // particular target address. Lookup order:
    //   1. Exact (ip:port) match — EPICS_CA_NAME_SERVERS hostname or
    //      EPICS_CA_TLS_SNI_MAP "ip:port=host" entry.
    //   2. Wildcard (ip:0) match — EPICS_CA_TLS_SNI_MAP "ip=host"
    //      entry (any port). lets operators map an IOC's IP
    //      once and have it apply to every port the search engine
    //      finds it on.
    //   3. Global EPICS_CA_TLS_SERVER_NAME fallback.
    //   4. (Caller's last fallback) IP literal as SNI.
    #[cfg(feature = "experimental-rust-tls")]
    let pick_sni = |addr: SocketAddr| -> Option<String> {
        if let Some(h) = sni_overrides.get(&addr) {
            return Some(h.clone());
        }
        let wildcard = SocketAddr::new(addr.ip(), 0);
        if let Some(h) = sni_overrides.get(&wildcard) {
            return Some(h.clone());
        }
        tls_server_name.clone()
    };

    loop {
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else { return };

                // BeaconArrivalNotify is a per-server UDP signal, not
                // tied to one priority circuit — fan it out to every
                // circuit for the server immediately (process_command
                // handles the fan-out). It never starts or waits on a
                // connect, so it skips the per-circuit queue entirely.
                let Some(circuit) = cmd_circuit_key(&cmd) else {
                    process_command(cmd, &mut connections, &server_writers, &in_flight, &event_tx);
                    continue;
                };

                // If a connect to this circuit is already in flight,
                // queue. Per-circuit FIFO is preserved because we push
                // at the tail and drain at completion.
                if queued_per_server.contains_key(&circuit) {
                    queued_per_server
                        .get_mut(&circuit)
                        .expect("just checked contains_key")
                        .push(cmd);
                    continue;
                }

                // Only CreateChannel triggers a connect. Other
                // commands either find the connection already
                // present or silently no-op via send_frame, which
                // matches pre-refactor behaviour for the rare
                // case where a command races a circuit teardown.
                if matches!(&cmd, TransportCommand::CreateChannel { .. }) {
                    let alive = connections
                        .get(&circuit)
                        .map(|c| !c._read_task.is_finished() && !c._write_task.is_finished())
                        .unwrap_or(false);
                    if !alive {
                        // Either no connection at all, or a
                        // stale entry whose tasks are already
                        // dead. Abort the dead pair before
                        // spawning a fresh connect.
                        if let Some(old) = connections.remove(&circuit) {
                            server_writers.remove(&circuit);
                            old._read_task.abort();
                            old._write_task.abort();
                        }
                        let (server_addr, priority) = circuit;
                        let event_tx_clone = event_tx.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let tls_clone = tls.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let sni = pick_sni(server_addr);
                        let in_flight_clone = in_flight.clone();
                        let last_rx_clone = last_rx_at.clone();
                        let identity_clone = client_identity.clone();
                        pending_connects.spawn(async move {
                            #[cfg(feature = "experimental-rust-tls")]
                            let conn = connect_server(
                                server_addr,
                                priority,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                                identity_clone,
                                tls_clone.as_ref(),
                                sni.as_deref(),
                            )
                            .await;
                            #[cfg(not(feature = "experimental-rust-tls"))]
                            let conn = connect_server(
                                server_addr,
                                priority,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                                identity_clone,
                            )
                            .await;
                            (circuit, conn)
                        });
                        // Queue this CreateChannel so its
                        // CREATE_CHAN frame goes out once the
                        // connection is up. Subsequent commands
                        // for this circuit will hit the
                        // `queued_per_server.contains_key` guard
                        // above and join the same queue.
                        queued_per_server.insert(circuit, vec![cmd]);
                        continue;
                    }
                }

                process_command(cmd, &mut connections, &server_writers, &in_flight, &event_tx);
            }
            Some(joined) = pending_connects.join_next() => {
                let (circuit, result) = match joined {
                    Ok(v) => v,
                    // Task panicked or was aborted before
                    // returning. Treat as "no result" — drop the
                    // queue (a panic in connect_server is a bug
                    // we can't recover from here) and continue.
                    Err(_) => continue,
                };
                let (server_addr, priority) = circuit;
                let queued = queued_per_server.remove(&circuit).unwrap_or_default();
                match result {
                    Some(conn) => {
                        server_writers.insert(
                            circuit,
                            DirectServerWriter {
                                write_tx: conn.write_tx.clone(),
                                pending_frames: conn.pending_frames.clone(),
                            },
                        );
                        connections.insert(circuit, conn);
                        // libca bhe-on-connect parity: announce the
                        // fresh circuit so the coordinator can ask the
                        // beacon monitor to reset its per-server EMA.
                        // Emit BEFORE replaying queued commands so the
                        // reset is observed before any subsequent
                        // anomaly classification on this circuit.
                        #[cfg(feature = "client")]
                        let _ = event_tx.send(TransportEvent::ServerConnected { server_addr });
                        for queued_cmd in queued {
                            process_command(
                                queued_cmd,
                                &mut connections,
                                &server_writers,
                                &in_flight,
                                &event_tx,
                            );
                        }
                    }
                    None => {
                        server_writers.remove(&circuit);
                        // Connect failed. Surface
                        // ChannelCreateFailed for each queued
                        // CreateChannel so the coordinator knows
                        // the channel can't progress on this
                        // circuit, and a single TcpClosed so the
                        // coordinator can clear any other state
                        // it kept on this circuit.
                        for queued_cmd in queued {
                            if let TransportCommand::CreateChannel { cid, .. } = queued_cmd {
                                let _ = event_tx.send(TransportEvent::ChannelCreateFailed { cid });
                            }
                        }
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                    }
                }
            }
        }
    }
}

/// Extract the target virtual-circuit key `(server_addr, priority)` from
/// any `TransportCommand`. Used by `run_transport_manager` to decide
/// whether a command needs to be queued behind a pending connect for
/// that circuit.
///
/// Returns `None` for `BeaconArrivalNotify`, which is a per-server UDP
/// signal that fans out to every priority circuit for the server rather
/// than targeting one — see the main loop and `process_command`.
fn cmd_circuit_key(cmd: &TransportCommand) -> Option<CircuitKey> {
    match cmd {
        TransportCommand::CreateChannel {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::ReadNotify {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Write {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::WriteNotify {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Subscribe {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Unsubscribe {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::ClearChannel {
            server_addr,
            priority,
            ..
        } => Some((*server_addr, *priority)),
        #[cfg(ca_beacon_monitor)]
        TransportCommand::BeaconArrivalNotify { .. } => None,
    }
}

/// Process a single command against an already-decided connection
/// state. Caller is responsible for ensuring any required connect
/// has completed (CreateChannel only — other commands rely on the
/// channel having been created successfully, which implies the
/// connection exists). All variants ultimately reduce to building
/// a CA frame and handing it to `send_frame`, except
/// `BeaconArrivalNotify` which forwards to the per-circuit
/// watchdog channel.
fn process_command(
    cmd: TransportCommand,
    connections: &mut HashMap<CircuitKey, ServerConnection>,
    server_writers: &DirectServerWriters,
    in_flight: &InFlightOps,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    match cmd {
        TransportCommand::CreateChannel {
            cid,
            pv_name,
            server_addr,
            priority,
        } => {
            let pv_payload = pad_string(&pv_name);
            let mut create_hdr = CaHeader::new(CA_PROTO_CREATE_CHAN);
            create_hdr.postsize = pv_payload.len() as u16;
            create_hdr.cid = cid;
            create_hdr.available = CA_MINOR_VERSION as u32;
            let mut frame = create_hdr.to_bytes().to_vec();
            frame.extend_from_slice(&pv_payload);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::ReadNotify {
            sid,
            data_type,
            count,
            ioid,
            server_addr,
            priority,
        } => {
            let peer_minor = peer_minor_of(connections, (server_addr, priority));
            let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = ioid;
            // C parity (`comQueSend.cpp:285`): extended form for
            // `nElem >= 0xffff`. See `build_read_notify_frame` in
            // client/mod.rs for the same boundary in the fast path.
            if count >= 0xFFFF {
                if hdr.set_payload_size(0, count, peer_minor).is_err() {
                    dispatch_read_error(in_flight, ioid, epics_base_rs::error::CaError::BadCount);
                    return;
                }
            } else {
                hdr.count = count as u16;
            }
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes_extended(),
                event_tx,
            );
        }
        TransportCommand::Write {
            sid,
            data_type,
            count,
            payload,
            server_addr,
            priority,
        } => {
            let peer_minor = peer_minor_of(connections, (server_addr, priority));
            // Same framing owner as the direct-writer path
            // (`CaChannel::build_write_frame`): C
            // `comQueSend::insertRequestWithPayLoad`.
            let Ok(frame) = crate::protocol::build_put_frame(
                CA_PROTO_WRITE,
                sid,
                data_type,
                count,
                None,
                payload,
                peer_minor,
            ) else {
                // No IOID on a fire-and-forget WRITE, so there is no
                // waiter to fail — libca's `ca_array_put` would have
                // returned ECA_BADCOUNT to the caller synchronously
                // (`comQueSend.cpp:313`). The channel-side gate does
                // that; reaching here means the request slipped past
                // it, so drop the frame rather than emit a header the
                // peer cannot parse.
                eprintln!(
                    "CA: {server_addr}: dropping WRITE for sid {sid}: \
                     libca would throw cacChannel::outOfBounds for this \
                     payload against a peer speaking CA minor {peer_minor} \
                     — ECA_BADCOUNT"
                );
                return;
            };
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::WriteNotify {
            sid,
            data_type,
            count,
            ioid,
            payload,
            server_addr,
            priority,
        } => {
            let peer_minor = peer_minor_of(connections, (server_addr, priority));
            let Ok(frame) = crate::protocol::build_put_frame(
                CA_PROTO_WRITE_NOTIFY,
                sid,
                data_type,
                count,
                Some(ioid),
                payload,
                peer_minor,
            ) else {
                if let Some((_, (_, reply_tx))) = in_flight.writes.remove(&ioid) {
                    let _ = reply_tx.send(Err(epics_base_rs::error::CaError::BadCount));
                }
                return;
            };
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::Subscribe {
            sid,
            data_type,
            count,
            subid,
            mask,
            server_addr,
            priority,
        } => {
            let peer_minor = peer_minor_of(connections, (server_addr, priority));
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
            hdr.postsize = 16;
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = subid;
            // C parity (`comQueSend.cpp:285`): extended form for
            // `nElem >= 0xffff`. Same boundary as READ_NOTIFY above.
            if count >= 0xFFFF {
                if hdr.set_payload_size(16, count, peer_minor).is_err() {
                    eprintln!(
                        "CA: {server_addr}: dropping EVENT_ADD for sid {sid}: \
                         extended header needed but peer speaks CA minor \
                         {peer_minor} (< 9) — ECA_BADCOUNT"
                    );
                    return;
                }
            } else {
                hdr.count = count as u16;
            }

            let mut mask_payload = [0u8; 16];
            mask_payload[12..14].copy_from_slice(&mask.to_be_bytes());

            let mut frame = hdr.to_bytes_extended();
            frame.extend_from_slice(&mask_payload);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::Unsubscribe {
            sid,
            subid,
            data_type,
            count,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_CANCEL);
            hdr.data_type = data_type;
            // Include the subscription's original
            // count, and serialise in extended form for counts
            // >= 0xFFFF. libca
            // `tcpiiu.cpp::subscriptionCancelRequest` routes through
            // `comQueSend::insertRequestHeader` which emits the
            // extended annex automatically. Pre-fix Rust truncated
            // the count to u16 and used `to_bytes()`, so a CANCEL
            // for a >= 65,535-element monitor lost the count and
            // diverged from libca byte-for-byte.
            let peer_minor = peer_minor_of(connections, (server_addr, priority));
            if hdr.set_payload_size(0, count, peer_minor).is_err() {
                // Unreachable: the matching EVENT_ADD could not have
                // been framed for this peer either.
                eprintln!(
                    "CA: {server_addr}: dropping EVENT_CANCEL for sid {sid}: \
                     extended header needed but peer speaks CA minor \
                     {peer_minor} (< 9) — ECA_BADCOUNT"
                );
                return;
            }
            hdr.cid = sid;
            hdr.available = subid;
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes_extended(),
                event_tx,
            );
        }
        TransportCommand::ClearChannel {
            cid,
            sid,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
            hdr.cid = sid;
            hdr.available = cid;
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes().to_vec(),
                event_tx,
            );
        }
        #[cfg(ca_beacon_monitor)]
        TransportCommand::BeaconArrivalNotify {
            server_addr,
            anomaly,
        } => {
            // Forward the beacon classification to the per-circuit
            // read loop. Healthy beacons refresh the watchdog
            // deadline (libca `beaconArrivalNotify`); anomaly
            // beacons set a sticky flag (libca
            // `beaconAnomalyNotify`) so the watchdog expires on
            // its own schedule and probes the circuit then,
            // rather than firing an immediate probe under load.
            //
            // one UDP beacon pets every priority circuit to
            // that server — fan out to all circuits whose key matches
            // `server_addr` (libca delivers `beaconArrivalNotify` to
            // each tcpiiu on the bhe's circuit list).
            for (key, conn) in connections.iter() {
                if key.0 == server_addr {
                    let _ = conn.beacon_arrival_tx.send(anomaly);
                }
            }
        }
    }
}

fn send_frame(
    connections: &mut HashMap<CircuitKey, ServerConnection>,
    server_writers: &DirectServerWriters,
    circuit: CircuitKey,
    frame: Vec<u8>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    let (server_addr, priority) = circuit;
    let failed = if let Some(conn) = connections.get(&circuit) {
        let pending = conn
            .pending_frames
            .load(std::sync::atomic::Ordering::Relaxed);
        if pending >= SEND_BACKPRESSURE_FRAMES {
            eprintln!("CA: {server_addr}: send buffer stalled ({pending} frames pending), closing");
            true
        } else {
            conn.pending_frames
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            conn.write_tx.send(frame).is_err()
        }
    } else {
        false
    };
    if failed {
        connections.remove(&circuit);
        server_writers.remove(&circuit);
        let _ = event_tx.send(TransportEvent::TcpClosed {
            server_addr,
            priority,
        });
    }
}

/// Peer minor protocol version of an established circuit, or 0 (pre-V49)
/// when the circuit is gone or has not yet announced its version.
///
/// Single reader of the per-circuit version published by `read_loop`, so
/// every request framed by [`process_command`] gates the extended header
/// on the same value libca feeds `insertRequestHeader` as `v49Ok`.
fn peer_minor_of(connections: &HashMap<CircuitKey, ServerConnection>, circuit: CircuitKey) -> u16 {
    connections
        .get(&circuit)
        .map(|c| c.server_minor.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

/// Build one CA identity frame — `CA_PROTO_CLIENT_NAME` (user name) or
/// `CA_PROTO_HOST_NAME` (host name) — carrying `value` as a NUL-padded
/// string payload.
///
/// Single source of the on-wire identity-frame shape, used by the
/// connect-time handshake ([`build_client_handshake`]), which is queued
/// before the peer's VERSION frame can have arrived — so its peer
/// version is unknown by construction and no extended header may be
/// emitted. C is in the same position and resolves it the same way:
/// `userNameSetRequest` / `hostNameSetRequest`
/// (`tcpiiu.cpp:1268,1303`) assert `postSize < 0xffff` before handing
/// the frame to `comQueSend::insertRequestHeader`, i.e. an identity
/// frame is always a plain 16-byte header. Names long enough to need
/// the annex cannot occur (the IOC caps a name at 512 bytes); a caller
/// that manages one anyway gets the payload clipped to the largest
/// aligned size the 16-bit postsize can carry rather than a frame the
/// peer cannot parse.
pub(crate) fn build_identity_frame(cmd: u16, value: &str) -> Vec<u8> {
    const MAX_IDENTITY_PAYLOAD: usize = 0xFFF8;
    let mut payload = pad_string(value);
    if payload.len() > MAX_IDENTITY_PAYLOAD {
        payload.truncate(MAX_IDENTITY_PAYLOAD);
        // keep the NUL terminator the receiver expects
        payload[MAX_IDENTITY_PAYLOAD - 1] = 0;
    }
    let mut hdr = CaHeader::new(cmd);
    hdr.postsize = payload.len() as u16;
    let mut frame = hdr.to_bytes().to_vec();
    frame.extend_from_slice(&payload);
    frame
}

/// Build the three-frame CA client handshake (VERSION, CLIENT_NAME,
/// HOST_NAME) for a circuit at `priority`.
///
/// C `tcpiiu` constructor (`modules/ca/src/client/tcpiiu.cpp:755-762`)
/// queues messages in this exact order:
///   1. versionMessage           → CA_PROTO_VERSION
///   2. userNameSetRequest       → CA_PROTO_CLIENT_NAME
///   3. hostNameSetRequest       → CA_PROTO_HOST_NAME
///
/// Pre-fix Rust emitted VERSION → HOST_NAME → CLIENT_NAME (the last two
/// swapped). Server `host_name_action` / `client_name_action` accept
/// either order in isolation, but ACF rules that consult both fields and
/// frame-byte-exact wire captures (Wireshark CA dissector, fuzzers)
/// diverge.
///
/// The VERSION message carries the requested CA priority in its
/// `m_dataType` field — libca `tcpiiu::versionMessage`
/// (`tcpiiu.cpp:1393-1397`) passes `priority` as the dataType and
/// `CA_MINOR_PROTOCOL_REVISION` as the count. Pre-fix Rust left dataType
/// at 0 (priorityDefault), so a server could not see the client's
/// requested priority.
fn build_client_handshake(priority: u8, identity: &super::types::ClientIdentitySlot) -> Vec<u8> {
    let mut handshake = Vec::new();
    let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
    version_hdr.count = CA_MINOR_VERSION;
    version_hdr.data_type = priority as u16;
    handshake.extend_from_slice(&version_hdr.to_bytes());
    // Snapshot the shared identity once. `CaClient::set_user_name` /
    // `set_host_name` mutate this slot at runtime, so a circuit formed
    // after a rename handshakes with the new names. Circuits already
    // established keep their identity — the IOC rejects a name change
    // once the circuit has created a channel.
    let (username, hostname) = {
        let id = identity.read();
        (id.user.clone(), id.host.clone())
    };
    handshake.extend_from_slice(&build_identity_frame(CA_PROTO_CLIENT_NAME, &username));
    handshake.extend_from_slice(&build_identity_frame(CA_PROTO_HOST_NAME, &hostname));
    handshake
}

#[allow(clippy::too_many_arguments)]
async fn connect_server(
    server_addr: SocketAddr,
    priority: u8,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
    identity: super::types::ClientIdentitySlot,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<&ClientTlsConfig>,
    #[cfg(feature = "experimental-rust-tls")] tls_server_name: Option<&str>,
) -> Option<ServerConnection> {
    tracing::debug!(server = %server_addr, "establishing TCP virtual circuit");
    // The dial, its socket options and its receive-queue probe are one step
    // and one seam — see `dial_ca`. It selects the transport at compile time,
    // so nothing below this line knows which one it got.
    let (stream, bytes_pending_in_os) = dial_ca(server_addr).await?;

    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let pending_frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Peer minor version: 0 (pre-V49) until this circuit's VERSION frame
    // lands in `read_loop`. Shared with the transport manager so request
    // framing can gate the extended header on it (C: `tcpiiu`'s
    // `minorProtocolVersion` → `insertRequestHeader`'s `v49Ok`).
    let server_minor = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
    // C `tcpiiu::unresponsiveCircuit` (`tcpiiu.cpp:899-940`): a single
    // circuit-level flag shared by BOTH the send and receive watchdogs.
    // `sendTimeoutNotify` (send stall) and `receiveTimeoutNotify` /
    // echo-timeout (recv stall) both funnel through
    // `unresponsiveCircuitNotify`, which sets it exactly once and marks
    // the channels unresponsive; `responsiveCircuitNotify` clears it on
    // the echo reply. Mirroring that single owner lets the write loop's
    // send-stall detection and the read loop's echo watchdog cooperate:
    // whichever observes the stall first performs the one-shot
    // `CircuitUnresponsive` transition, and the read loop's data-arrival
    // path performs the sole `CircuitResponsive` recovery — so a stall
    // first seen on the send side still recovers when replies resume on
    // the read side. `UnresponsiveGate` guards the (test + emit) pair
    // under one mutex, exactly as C guards it under the cac lock, so a
    // `CircuitResponsive` from the read loop can never be enqueued ahead
    // of a `CircuitUnresponsive` still in flight from the write loop.
    let unresponsive = std::sync::Arc::new(UnresponsiveGate::new());
    let (beacon_arrival_tx, beacon_arrival_rx) = mpsc::unbounded_channel::<bool>();
    // A build with no beacon monitor has nothing that will ever send on
    // this. Dropping the sender here puts `read_loop`'s arrival arm in the
    // state a shutdown leaves it in — closed on first poll, then disarmed —
    // rather than leaving a channel alive that no code can feed. (The arm
    // itself cannot be `cfg`'d: `tokio::select!` does not accept attributes
    // on a branch.) That is `client-core`, and equally the reactor-free
    // `exec_backend`, where the monitor's UDP socket cannot exist.
    #[cfg(not(ca_beacon_monitor))]
    drop(beacon_arrival_tx);

    // Build initial CA handshake.
    let handshake = build_client_handshake(priority, &identity);
    pending_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = write_tx.send(handshake);

    // Spawn read/write tasks. The TLS path wraps the TCP stream in a
    // `tokio_rustls::TlsStream` first; the plaintext path splits the
    // raw TcpStream. Both feed identical-shape generic loops.
    #[cfg(feature = "experimental-rust-tls")]
    let (read_task, write_task) = if let Some(tls_cfg) = tls {
        // Prefer the operator-supplied SNI / cert-hostname-verification
        // name (e.g. EPICS_CA_TLS_SERVER_NAME=ioc.example.com); fall back
        // to the server's IP literal when nothing is configured. The IP
        // literal only validates against IP-bound certs, so hostname-bound
        // certs require the explicit override.
        let sni_str: String = match tls_server_name {
            Some(n) if !n.is_empty() => n.to_owned(),
            _ => server_addr.ip().to_string(),
        };
        let server_name = match tokio_rustls::rustls::pki_types::ServerName::try_from(sni_str) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(server = %server_addr, error = %e, "invalid TLS server name");
                return None;
            }
        };
        let connector = TlsConnector::from(tls_cfg.clone());
        // cap the client-side TLS handshake. A misbehaving (or
        // hostile) server that completes TCP but stalls during
        // ServerHello would otherwise leave the client awaiting
        // forever. Pairs with the existing TCP-connect timeout above.
        // 10s default — long enough for a normal cert exchange, short
        // enough to fall through to the next NAME_SERVER candidate.
        let hs_timeout = tls_handshake_timeout();
        let tls_stream =
            // Seam, for the same reason as the two pumps below: this is on the
            // circuit path, and a tokio timer there panics on a target with no
            // reactor. Not reached in today's target build (the TLS feature is
            // off there), but leaving one tokio timer on the circuit path is
            // what re-opens the family.
            match epics_base_rs::runtime::task::timeout(
                hs_timeout,
                connector.connect(server_name, stream),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(server = %server_addr, error = %e, "TLS handshake failed");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(server = %server_addr,
                    timeout = ?hs_timeout, "TLS handshake timed out");
                    return None;
                }
            };
        tracing::debug!(server = %server_addr, "TLS handshake complete");
        let (reader, writer) = tokio::io::split(tls_stream);
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            priority,
            event_tx.clone(),
            pending_frames.clone(),
            unresponsive.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
            unresponsive.clone(),
            bytes_pending_in_os.clone(),
            server_minor.clone(),
        ));
        (read_task, write_task)
    } else {
        let (reader, writer) = split_circuit(stream);
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            priority,
            event_tx.clone(),
            pending_frames.clone(),
            unresponsive.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
            unresponsive.clone(),
            bytes_pending_in_os.clone(),
            server_minor.clone(),
        ));
        (read_task, write_task)
    };

    #[cfg(not(feature = "experimental-rust-tls"))]
    let (read_task, write_task) = {
        let (reader, writer) = split_circuit(stream);
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            priority,
            event_tx.clone(),
            pending_frames.clone(),
            unresponsive.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
            unresponsive.clone(),
            bytes_pending_in_os.clone(),
            server_minor.clone(),
        ));
        (read_task, write_task)
    };

    Some(ServerConnection {
        write_tx,
        pending_frames,
        server_minor,
        #[cfg(ca_beacon_monitor)]
        beacon_arrival_tx,
        _read_task: read_task,
        _write_task: write_task,
    })
}

/// Single owner of a virtual circuit's unresponsive state.
///
/// C funnels both watchdogs through one `tcpiiu::unresponsiveCircuit`
/// bool guarded by the cac mutex, so the flag flip and its
/// `genLocalExcep(ECA_UNRESPTMO)` / `responsiveCircuitNotify` are atomic
/// with respect to each other (`tcpiiu.cpp:861-940`). The send watchdog
/// (`write_loop`) and the receive echo watchdog (`read_loop`) run on
/// separate tasks that share one `event_tx`, so a bare `AtomicBool` swap
/// followed by a *separate* `event_tx.send` is NOT atomic: the write loop
/// could win `swap(true)`, be preempted before its send, and the read
/// loop could then win `swap(false)` and enqueue `CircuitResponsive`
/// ahead of the still-pending `CircuitUnresponsive`. The coordinator would
/// apply Responsive (a no-op on not-yet-unresponsive channels) before
/// Unresponsive, wedging the channels Unresponsive with revoked access on
/// a live circuit. Guarding the (test + emit) pair with one mutex makes
/// each transition indivisible, so the two events can never be delivered
/// out of order.
struct UnresponsiveGate {
    state: std::sync::Mutex<bool>,
    /// C `tcpiiu::_receiveThreadIsBusy` (`tcpiiu.cpp:494/526`): true while
    /// `read_loop` is processing a just-arrived message batch. C's
    /// `tcpSendWatchdog::expire` reads it under the mutex and RESTARTS the
    /// send watchdog instead of calling `sendTimeoutNotify` when the recv
    /// thread is busy (`tcpSendWatchdog.cpp:48-50`) — a circuit that is
    /// actively receiving is demonstrably alive, so a send stall must not
    /// mark it unresponsive (which would fail in-flight IO with
    /// ECA_UNRESPTMO). Lives on the gate because the gate is already the
    /// shared owner of the circuit's liveness state seen by both loops.
    recv_busy: std::sync::atomic::AtomicBool,
}

impl UnresponsiveGate {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(false),
            recv_busy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `read_loop` marks itself busy across message processing and idle
    /// while blocked in the socket read. Best-effort liveness hint (a plain
    /// relaxed flag, exactly like C's mutex-guarded bool), so `Relaxed`.
    fn set_recv_busy(&self, busy: bool) {
        self.recv_busy
            .store(busy, std::sync::atomic::Ordering::Relaxed);
    }

    /// `write_loop`'s send-stall arm consults this to skip the unresponsive
    /// transition when the recv thread is actively receiving.
    fn recv_busy(&self) -> bool {
        self.recv_busy.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Mark the circuit unresponsive and emit `CircuitUnresponsive` exactly
    /// once per episode — only the `false → true` transition emits, so the
    /// send watchdog and the read echo watchdog cannot double-post. Mirrors
    /// C's `if (!unresponsiveCircuit)` guard (`tcpiiu.cpp:906`).
    fn mark_unresponsive(
        &self,
        event_tx: &mpsc::UnboundedSender<TransportEvent>,
        server_addr: SocketAddr,
        priority: u8,
    ) {
        let mut state = self.state.lock().unwrap();
        if !*state {
            *state = true;
            let _ = event_tx.send(TransportEvent::CircuitUnresponsive {
                server_addr,
                priority,
            });
        }
    }

    /// Clear the unresponsive state on data arrival and emit the sole
    /// `CircuitResponsive` — only the `true → false` transition emits, so a
    /// circuit that was never marked stays quiet. Mirrors C's
    /// `if (this->unresponsiveCircuit)` guard in `responsiveCircuitNotify`
    /// (`tcpiiu.cpp:867`).
    fn mark_responsive(
        &self,
        event_tx: &mpsc::UnboundedSender<TransportEvent>,
        server_addr: SocketAddr,
        priority: u8,
    ) {
        let mut state = self.state.lock().unwrap();
        if *state {
            *state = false;
            let _ = event_tx.send(TransportEvent::CircuitResponsive {
                server_addr,
                priority,
            });
        }
    }

    /// Test-only read of the current unresponsive state. Gated exactly like
    /// its only callers (`write_loop_timeout_tests`), which the virtual clock
    /// keeps out of the feature-ON suite.
    #[cfg(all(test, not(feature = "rtems-exec-model")))]
    fn is_unresponsive(&self) -> bool {
        *self.state.lock().unwrap()
    }
}

async fn write_loop<W: AsyncWrite + Unpin + Send + 'static>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    server_addr: SocketAddr,
    priority: u8,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    pending_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    unresponsive: std::sync::Arc<UnresponsiveGate>,
) {
    // Send watchdog. C `tcpSendWatchdog` (`libca/tcpSendWatchdog.cpp:43-64`)
    // fires after `connTMO` (`EPICS_CA_CONN_TMO`, default 30 s) and calls
    // `iiu.sendTimeoutNotify` → `unresponsiveCircuitNotify`
    // (`tcpiiu.cpp:879-940`): it marks the circuit unresponsive, arms an
    // echo probe, and KEEPS the socket. The send side never tears the
    // circuit down — a permanently dead circuit is closed by the RECEIVE
    // watchdog (echo-timeout in `read_loop`), never the send watchdog.
    //
    // Matching that requires not abandoning a partial CA frame on a stall.
    // `write_all` cannot: `timeout(.., write_all(&batch))` cancels the
    // future mid-frame and loses the bytes-written count, so the stream
    // could only be discarded (the previous behaviour). Instead we drive
    // the flush ourselves with `writer.write(&batch[written..])` and carry
    // `written` across stalls. `timeout` reports `Err` only when the inner
    // `write` was genuinely `Pending`, and a `Pending` `poll_write` wrote
    // 0 bytes per the `AsyncWrite` contract — so on a send-watchdog expiry
    // `written` is exact and we resume the SAME batch from that offset. No
    // byte is ever re-sent, so the server parser never desyncs. A real
    // socket error (`Err`) or a 0-byte accept still closes (`TcpClosed`):
    // those are a dead socket, not a slow one, mirroring C's `flushToWire`
    // failure → `shutdown(SHUT_WR)` teardown (`tcpiiu.cpp:168-176`).
    let send_timeout = connection_timeout();
    let mut batch = Vec::with_capacity(4096);
    while let Some(frame) = rx.recv().await {
        let mut drained: usize = 1;
        batch.clear();
        batch.extend_from_slice(&frame);
        // Coalesce all queued frames into a single flush.
        while let Ok(frame) = rx.try_recv() {
            batch.extend_from_slice(&frame);
            drained += 1;
        }
        // Flush the whole batch, resuming across send-watchdog stalls
        // without abandoning a partial frame.
        let mut written = 0usize;
        while written < batch.len() {
            // The `runtime::task` seam, not `tokio::time::timeout`: on the
            // RTEMS target this pump runs with no tokio reactor anywhere in
            // the process, and a tokio timer panics the task rather than
            // firing (§11.1).
            match epics_base_rs::runtime::task::timeout(
                send_timeout,
                writer.write(&batch[written..]),
            )
            .await
            {
                Ok(Ok(0)) => {
                    // Peer will accept no more bytes — dead socket.
                    let _ = event_tx.send(TransportEvent::TcpClosed {
                        server_addr,
                        priority,
                    });
                    return;
                }
                Ok(Ok(n)) => {
                    written += n;
                }
                Ok(Err(_)) => {
                    // True socket error — circuit is dead.
                    let _ = event_tx.send(TransportEvent::TcpClosed {
                        server_addr,
                        priority,
                    });
                    return;
                }
                Err(_) => {
                    // Send watchdog: no write progress within `connTMO`.
                    // C `sendTimeoutNotify` → `unresponsiveCircuitNotify`:
                    // mark the circuit unresponsive (once, via the gate
                    // shared with the read loop's echo watchdog) and KEEP
                    // the socket. The cancelled `write` was `Pending`, so
                    // `written` is exact — loop and resume the batch from
                    // `written`. Recovery (`CircuitResponsive`) and the
                    // dead-circuit close are both owned by `read_loop`; on
                    // teardown `ServerConnection::drop` aborts this task,
                    // so a forever-stalled write cannot leak.
                    //
                    // But first mirror C `tcpSendWatchdog::expire`: if the
                    // recv thread is mid-processing a message the circuit is
                    // demonstrably alive, so RESTART the watchdog (re-poll
                    // the same write below) instead of marking unresponsive
                    // (`tcpSendWatchdog.cpp:48-50`). The read echo watchdog
                    // remains the sole owner of declaring a truly dead
                    // circuit, so nothing is lost by deferring here.
                    if !unresponsive.recv_busy() {
                        unresponsive.mark_unresponsive(&event_tx, server_addr, priority);
                    }
                }
            }
        }
        // Whole batch is on the wire — decrement the backpressure counter.
        // `pending_frames` decides when `send_frame` should treat a stalled
        // circuit as disconnected. `fetch_sub` via a saturating CAS loop
        // never loses a concurrent `send_frame::fetch_add` (a plain
        // `load`+`store` would) and never wraps on the occasional
        // `read_loop` echo frame that bypassed `send_frame`'s increment.
        let mut current = pending_frames.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(drained);
            match pending_frames.compare_exchange_weak(
                current,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    server_addr: SocketAddr,
    priority: u8,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut beacon_arrival_rx: mpsc::UnboundedReceiver<bool>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
    unresponsive: std::sync::Arc<UnresponsiveGate>,
    bytes_pending_in_os: OsRecvQueueProbe,
    server_minor: std::sync::Arc<std::sync::atomic::AtomicU16>,
) {
    // Helper: emit an echo (or pre-v4.3 READ_SYNC) request. Used on idle
    // expiry, and again on echo timeout (C `unresponsiveCircuitNotify`
    // re-arms `echoRequestPending`, `tcpiiu.cpp:908`).
    fn send_echo(
        write_tx: &mpsc::UnboundedSender<Vec<u8>>,
        server_minor_version: u16,
    ) -> Result<(), ()> {
        let cmd = if server_minor_version >= 3 {
            CA_PROTO_ECHO
        } else {
            CA_PROTO_READ_SYNC
        };
        let echo_hdr = CaHeader::new(cmd);
        write_tx.send(echo_hdr.to_bytes().to_vec()).map_err(|_| ())
    }

    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();
    // C `tcpiiu::processIncoming`'s receive-side body limit and its
    // ignore-don't-close policy — see [`RecvBodyPolicy`], shared with the
    // name-service circuit's reader.
    let mut body_policy = RecvBodyPolicy::new();
    let idle_timeout = Duration::from_secs(echo_idle_secs());
    let echo_timeout = Duration::from_secs(ECHO_TIMEOUT_SECS);
    let mut echo_pending = false;
    // libca Issue #190 (laptop-suspend stall): the OS may pause the
    // tokio reactor for many minutes during system suspend. On
    // resume, `Instant::now()` jumps forward by ~the suspend
    // duration. The Sleep fires immediately (deadline in the past)
    // and sends an echo — but the TCP socket may be half-open and
    // we'd otherwise wait the full 5 s echo timeout to find out.
    // Track wall-clock between loop iterations; if it skips more than
    // `SUSPEND_THRESHOLD` (3× idle_timeout — large enough to ignore
    // ordinary scheduling jitter, small enough to fire on a real
    // suspend of even a few minutes) we use the abbreviated echo
    // timeout so recovery completes in seconds instead of tens.
    const SUSPEND_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
    let suspend_threshold = idle_timeout.saturating_mul(3).max(Duration::from_secs(60));
    // Anchor for suspend detection — refreshed at the top of every
    // loop iteration. Initialized at the loop entry; first iteration
    // overwrites it before the wall-clock skip is consulted.
    let mut last_loop_at;
    // libca `tcpRecvWatchdog::beaconAnomaly` flag. Set when the
    // beacon monitor classifies a beacon as anomalous — fresh sequence
    // or off-band period (`bhe.cpp:226-262`); suppresses subsequent
    // healthy-beacon watchdog refreshes so the deadline expires on
    // its own schedule. Cleared on any data arrival from the server.
    let mut beacon_anomaly = false;
    // C `tcpRecvWatchdog`'s timer armed/cancelled state. `expire` returns
    // `noRestart` once the echo probe times out (`tcpRecvWatchdog.cpp:81`)
    // and `unresponsiveCircuitNotify` cancels the timer outright
    // (`tcpiiu.cpp:915-921`), so the watchdog stops firing on an
    // already-unresponsive circuit; the socket stays open and any byte from
    // the server re-arms it. Distinct from the shared `unresponsive` flag,
    // which is the circuit-level state — also set by the send watchdog in
    // `write_loop` — and guards the one-shot `CircuitUnresponsive` /
    // `CircuitResponsive` events.
    let mut watchdog_armed = true;
    // libca flow control (C `tcpRecvThread::run`, `tcpiiu.cpp:543-572`).
    // `contig_recv_msg_count` counts consecutive receive frames that each
    // left bytes still unread in the OS socket buffer; once it reaches
    // `max_contiguous_frames()` the circuit is "busy" and we ask the server
    // to stop sending monitors. The moment the socket reads clean the count
    // resets and, if busy, flow control lifts immediately — C's comment:
    // "if no bytes are pending then we must immediately switch off flow
    // control w/o waiting for more data to arrive" (`tcpiiu.cpp:559-561`).
    //
    // C splits this across two flags — `busyStateDetected` (recv thread) and
    // `flowControlActive` (send thread) — only because two threads observe
    // it. Here one loop both detects the transition and queues the frame, so
    // one flag carries the whole state and every edge emits exactly one
    // request.
    let max_contig_frames = crate::protocol::max_contiguous_frames();
    let mut contig_recv_msg_count: usize = 0;
    let mut flow_control_active = false;
    let mut server_minor_version: u16 = 0;
    let mut beacon_rx_open = true;
    // C `claim_ciu_reply` (`rsrv/camessage.c:1149-1175`) emits the
    // CA_PROTO_ACCESS_RIGHTS frame BEFORE the CA_PROTO_CREATE_CHAN
    // reply on the same TCP stream. The coordinator's
    // `AccessRightsChanged` handler at `mod.rs:2531` looks up the
    // channel by cid — but the channel doesn't exist in the
    // coordinator's map until `ChannelCreated` arrives second. So
    // the access bits get silently dropped, and the
    // `ChannelCreated` event hard-coded `AccessRights::from_u32(0x3)`
    // (full READ+WRITE) regardless of what the server actually
    // granted. Result: a Rust client against a read-only PV could
    // attempt writes that the server rejects later (ECA_NOWTACCESS),
    // instead of refusing them client-side from the access cache.
    //
    // Stash by cid; consumed when the matching CREATE_CHAN reply
    // arrives. If multiple ACCESS_RIGHTS frames arrive between
    // CREATE_CHAN cycles (rare but legal — server may emit
    // mid-stream on ACF reload), only the most recent is kept.
    let mut pending_access: std::collections::HashMap<u32, AccessRights> =
        std::collections::HashMap::new();
    // cids the server has acknowledged via CREATE_CHAN. An
    // ACCESS_RIGHTS frame for a known cid is a *post-create* update
    // (ACF reload, server-side rule change) and must fire the event.
    // An ACCESS_RIGHTS frame for an unknown cid is a *pre-create*
    // stash — the matching CREATE_CHAN reply consumes it and the
    // ChannelCreated event already carries the access, so no
    // AccessRightsChanged is needed in that path. Pre-fix Rust
    // emitted the event in both cases; combined with the stash
    // cap, a stray-ACCESS_RIGHTS-flood from a hostile server loaded
    // the unbounded event_tx mpsc one message per stray frame even
    // though the coordinator's downstream filter dropped them all.
    let mut known_cids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // rate-limit the cap-hit warning so a hostile flood does
    // not also flood the logs. One warn per circuit lifetime is
    // enough — the metric `ca_client_pending_access_evictions_total`
    // carries the running count for observability.
    let mut cap_warned = false;

    // The watchdog deadline is the state; the sleep future is derived from
    // it inside the `select!` below. This is what makes the libca model
    // expressible: we extend the watchdog deadline on healthy beacons and on
    // data arrival, and we leave it untouched on anomaly beacons so the timer
    // still expires on its original schedule.
    //
    // An **absolute** `std::time::Instant` re-slept each iteration, not a
    // pinned `tokio::time::Sleep` mutated by `Sleep::reset`: the seam's
    // `sleep_until` has no `reset`, and it does not need one — re-deriving a
    // future for the *same* absolute deadline is the same deadline. Leaving
    // `deadline` alone is still "do not touch the watchdog". The seam is not
    // optional here: on the RTEMS target this loop runs with no tokio reactor
    // in the process, and `tokio::time::sleep_until` panics the task instead
    // of firing (§11.1).
    let mut deadline = epics_base_rs::runtime::task::Instant::now() + idle_timeout;

    loop {
        // Refresh the suspend-detection anchor at the top of each
        // loop iteration. When the sleep branch wakes, `wall_skip =
        // SystemTime::now() - last_loop_at` reveals whether the host
        // was suspended during the await — sleep duration on a live
        // system stays bounded by `idle_timeout`/`echo_timeout`, so a
        // skip far beyond that is a strong suspend signal.
        last_loop_at = std::time::SystemTime::now();
        // Not processing a message while parked in `select!` (blocked in
        // the socket read / watchdog / beacon wait). C `_receiveThreadIsBusy`
        // is false here; the send watchdog may mark the circuit unresponsive
        // if a send also stalls. Set true only across message processing
        // below (after data arrives).
        unresponsive.set_recv_busy(false);
        let n = tokio::select! {
            // No `biased;` — let tokio randomize. With three
            // branches (beacon arrival, sleep expiry, data read)
            // a fixed priority would risk starving whichever lost
            // — initially we tried `biased` favoring the beacon
            // branch and realized that under a beacon flood it
            // could starve the data path, which is exactly the
            // failure mode we wanted to avoid. tokio's default
            // randomized polling gives uniform fairness without
            // any cleverness on our part.
            arrival = beacon_arrival_rx.recv(), if beacon_rx_open => {
                match arrival {
                    Some(true) => {
                        // libca beaconAnomalyNotify: set sticky flag,
                        // do NOT touch the deadline. The watchdog
                        // will expire on schedule and probe then —
                        // matches libca's "be careful about using
                        // beacons to reset the connection time out
                        // watchdog until we have received a ping
                        // response" comment in tcpRecvWatchdog.cpp.
                        beacon_anomaly = true;
                    }
                    Some(false) => {
                        // libca beaconArrivalNotify: refresh the
                        // deadline only when we trust beacons (no
                        // anomaly outstanding) and aren't already
                        // probing.
                        if !beacon_anomaly && !echo_pending {
                            deadline = epics_base_rs::runtime::task::Instant::now() + idle_timeout;
                        }
                    }
                    None => {
                        // Transport manager dropped the sender —
                        // shutdown in progress. Stop polling this
                        // branch so we don't busy-loop on Ready(None).
                        beacon_rx_open = false;
                    }
                }
                continue;
            }
            // Watchdog deadline expired. Disabled while the watchdog is
            // cancelled (C: unresponsive circuit, timer not restarted).
            _ = epics_base_rs::runtime::task::sleep_until(deadline), if watchdog_armed => {
                // libca Issue #190: detect suspend wake. If wall-clock
                // skipped far more than expected for this sleep, the
                // tokio reactor was paused (laptop suspend / VM stop).
                // Shorten the echo probe so recovery is seconds, not
                // tens of seconds.
                let now_wall = std::time::SystemTime::now();
                let wall_skip = now_wall
                    .duration_since(last_loop_at)
                    .unwrap_or(Duration::ZERO);
                let suspend_wake = wall_skip >= suspend_threshold;
                if echo_pending {
                    // Echo timeout. C `tcpRecvWatchdog::expire` with
                    // `probeResponsePending` set calls `receiveTimeoutNotify`
                    // and returns `noRestart` (`tcpRecvWatchdog.cpp:54-81`).
                    // That routes to `tcpiiu::unresponsiveCircuitNotify`
                    // (`tcpiiu.cpp:890-941`), which marks the circuit
                    // unresponsive, re-arms `echoRequestPending` so one more
                    // probe departs on the send thread, cancels BOTH
                    // watchdogs, raises `ECA_UNRESPTMO` — and KEEPS the
                    // socket. The circuit is torn down only on a genuine
                    // socket error (`tcpiiu.cpp:586-601`), i.e. the
                    // `Ok(0) | Err(_)` read arm below.
                    //
                    // So: perform the one-shot unresponsive transition
                    // (shared with the send watchdog via the gate — only the
                    // winning transition emits), emit the trailing probe, and
                    // DISARM the watchdog. The data-arrival path below is the
                    // recovery: it re-arms the deadline and emits the sole
                    // `CircuitResponsive`. A server that goes quiet for
                    // minutes and comes back keeps its circuit, its channels
                    // and its subscriptions, with no re-search.
                    unresponsive.mark_unresponsive(&event_tx, server_addr, priority);
                    if send_echo(&write_tx, server_minor_version).is_err() {
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                        return;
                    }
                    watchdog_armed = false;
                    continue;
                }
                // Idle expired — send echo heartbeat. The deadline
                // path itself doesn't read `beacon_anomaly`; the
                // flag's job is upstream, in the beacon-arrival
                // branch, where it gates whether healthy beacons
                // refresh the deadline. By the time we get here on
                // an anomaly-flagged circuit, that gating has
                // already kept the deadline at its original value
                // long enough for it to expire on the schedule it
                // would have had without any beacons at all.
                if send_echo(&write_tx, server_minor_version).is_err() {
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                    return;
                }
                echo_pending = true;
                let probe = if suspend_wake {
                    tracing::info!(
                        server = %server_addr,
                        wall_skip_secs = wall_skip.as_secs(),
                        "suspend wake detected; probing with shortened echo timeout"
                    );
                    SUSPEND_PROBE_TIMEOUT
                } else {
                    echo_timeout
                };
                deadline = epics_base_rs::runtime::task::Instant::now() + probe;
                continue;
            }
            // Data from the server.
            read_result = reader.read(&mut buf) => {
                match read_result {
                    Ok(0) | Err(_) => {
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                        return;
                    }
                    Ok(n) => n,
                }
            }
        };

        // Data received — circuit is alive. Mirrors libca
        // `messageArrivalNotify`: clear flags and refresh deadline.
        // Mark the recv thread busy across the message processing that
        // follows (C `_receiveThreadIsBusy = true`, `tcpiiu.cpp:494`);
        // the next loop-top store clears it (C sets false at :526). While
        // set, a concurrent send stall in `write_loop` restarts its
        // watchdog rather than marking this live circuit unresponsive.
        unresponsive.set_recv_busy(true);
        echo_pending = false;
        beacon_anomaly = false;
        // Re-arm the watchdog (C `messageArrivalNotify` restarts the timer;
        // an unresponsive circuit's cancelled timer comes back here).
        watchdog_armed = true;
        deadline = epics_base_rs::runtime::task::Instant::now() + idle_timeout;
        // Phase D: bump the per-server "last RX" stamp before any
        // protocol parsing so that even ECHO replies and frames the
        // parser later rejects still count as proof of liveness.
        // Read by `ca_receive_watchdog_delay` via the coordinator.
        last_rx_at.insert((server_addr, priority), std::time::Instant::now());
        // Recovery: clear the shared unresponsive state and emit the sole
        // `CircuitResponsive`, whether the stall was first seen on the send
        // watchdog (`write_loop`) or on this read loop's echo watchdog.
        // Mirrors C `responsiveCircuitNotify`'s `if (unresponsiveCircuit)`
        // guard (`tcpiiu.cpp:867`).
        unresponsive.mark_responsive(&event_tx, server_addr, priority);

        // Flow control is evaluated at the BOTTOM of this iteration, after the
        // frame's messages have been processed — that is where C samples
        // `bytesArePendingInOS()` (`tcpiiu.cpp:543-546`).
        accumulated.extend_from_slice(&buf[..n]);

        // Bytes owed to an already-refused oversize message are consumed
        // before framing resumes (C `recvQue.removeBytes`, see
        // [`RecvBodyPolicy::drain_refused`]).
        if body_policy.drain_refused(&mut accumulated) {
            continue;
        }

        let mut offset = 0;
        loop {
            // The shared framing step (`next_frame`) — "await more bytes" and
            // "this peer is definitively malformed" are its answer, not this
            // loop's, so the name-service circuit's reader cannot drift away
            // from these rules again (§6 C2).
            let (hdr, hdr_size, actual_post) = match next_frame(&accumulated[offset..]) {
                Frame::Incomplete => break,
                Frame::Malformed(e) => {
                    // C `tcpiiu.cpp::processIncoming:1197-1202` closes the
                    // connection on either a header it cannot parse or a
                    // misaligned `m_postsize`. Silently rounding the latter
                    // via `align8` (the prior behavior) lets a malicious
                    // server desync our framer; drop the circuit so the
                    // reconnect loop rebuilds from a clean state.
                    eprintln!("CA: {server_addr}: {e}, closing");
                    let _ = event_tx.send(TransportEvent::TcpClosed {
                        server_addr,
                        priority,
                    });
                    return;
                }
                Frame::Header {
                    hdr,
                    hdr_size,
                    body_len,
                } => (hdr, hdr_size, body_len),
            };
            let msg_len = hdr_size + actual_post;

            // C `tcpiiu::processIncoming` (`tcpiiu.cpp:1269-1283`): a body the
            // circuit's cache cannot hold is IGNORED — logged once, drained
            // with `recvQue.removeBytes`, circuit kept. Unreachable with C's
            // default (no limit), which is the whole point: a C client reads
            // a 33 MB waveform from a C IOC without complaint, so ours must
            // too. The rule itself lives in [`RecvBodyPolicy`].
            if body_policy.refuses(server_addr, actual_post) {
                let present = accumulated.len() - offset;
                if msg_len <= present {
                    offset += msg_len;
                    continue;
                }
                // The body is still arriving: consume what is here and carry
                // the remainder into the next reads (C keeps `curDataBytes`
                // and returns true to await more).
                body_policy.owe(msg_len - present);
                offset = accumulated.len();
                break;
            }

            if offset + msg_len > accumulated.len() {
                break;
            }

            let data_start = offset + hdr_size;
            let data_end = data_start + actual_post;

            // Defense-in-depth: verify payload is within buffer bounds
            // even though msg_len check above should guarantee this.
            if data_end > accumulated.len() {
                eprintln!("CA: {server_addr}: payload exceeds buffer bounds, skipping");
                break;
            }

            match hdr.cmmd {
                CA_PROTO_VERSION => {
                    server_minor_version = hdr.count;
                    // Publish to the transport manager: request framing
                    // gates the extended header on the peer's version
                    // (C `insertRequestHeader`'s `v49Ok`).
                    server_minor.store(hdr.count, std::sync::atomic::Ordering::Relaxed);
                    let _ = event_tx.send(TransportEvent::ServerVersion {
                        server_addr,
                        priority,
                        minor_version: hdr.count,
                    });
                }
                CA_PROTO_ACCESS_RIGHTS => {
                    let access = AccessRights::from_u32(hdr.available);
                    // Stash for the next CREATE_CHAN reply on this
                    // cid (C orders ACCESS_RIGHTS first; the
                    // coordinator's update-by-cid is a no-op
                    // pre-channel).
                    //
                    // bound the stash size. C `libca/cac.cpp:
                    // 1121-1136` `accessRightsRespAction` looks up by
                    // m_cid and silently returns if not found — never
                    // accumulates state. Pre-fix Rust grew the map on
                    // every ACCESS_RIGHTS frame, so a misbehaving /
                    // hostile server emitting ACCESS_RIGHTS for cids
                    // that never get named in CREATE_CHAN leaked one
                    // entry per frame for the circuit's lifetime.
                    // 1024 is well past the per-client channel cap
                    // any realistic deployment hits; well below the
                    // memory pressure threshold.
                    // post-create ACCESS_RIGHTS goes straight
                    // to the coordinator as an update event; the
                    // pre-create path stashes for the CREATE_CHAN
                    // consumer (which folds it into ChannelCreated).
                    if known_cids.contains(&hdr.cid) {
                        let _ = event_tx.send(TransportEvent::AccessRightsChanged {
                            cid: hdr.cid,
                            access,
                        });
                    } else {
                        // bound the stash size. C
                        // `libca/cac.cpp:1121-1136`
                        // `accessRightsRespAction` looks up by m_cid
                        // and silently returns if not found — never
                        // accumulates. 1024 is well past the per-
                        // client channel cap any realistic deployment
                        // hits; well below memory pressure.
                        const PENDING_ACCESS_CAP: usize = 1024;
                        if pending_access.len() >= PENDING_ACCESS_CAP {
                            if let Some(&victim) = pending_access.keys().next() {
                                pending_access.remove(&victim);
                                metrics::counter!("ca_client_pending_access_evictions_total")
                                    .increment(1);
                                // log the cap-hit ONCE per
                                // circuit so operators can correlate
                                // with a misbehaving server. C never
                                // accumulates so this condition can't
                                // exist in C; we mirror C's silent-
                                // on-unknown-cid behaviour at steady
                                // state but surface the new failure
                                // mode (cap exceeded) at warn level.
                                if !cap_warned {
                                    cap_warned = true;
                                    tracing::warn!(
                                        target: "epics_ca_rs::client::transport",
                                        cap = PENDING_ACCESS_CAP,
                                        "pending_access cap reached — server is emitting \
                                         ACCESS_RIGHTS for cids no CREATE_CHAN names; oldest \
                                         entry evicted. Further evictions are silent; see \
                                         metric ca_client_pending_access_evictions_total"
                                    );
                                }
                            }
                        }
                        pending_access.insert(hdr.cid, access);
                    }
                }
                CA_PROTO_CREATE_CHAN => {
                    // Consume the stashed ACCESS_RIGHTS for this cid
                    // if any (C `claim_ciu_reply` always emits one
                    // first; falls back to NoAccess if missing —
                    // defensive default since we can't assume
                    // RW on an open channel).
                    let access = pending_access
                        .remove(&hdr.cid)
                        .unwrap_or(AccessRights::from_u32(0));
                    // now that the server has named this cid,
                    // subsequent ACCESS_RIGHTS frames for it are
                    // legitimate post-create updates that must fire
                    // AccessRightsChanged.
                    known_cids.insert(hdr.cid);
                    let _ = event_tx.send(TransportEvent::ChannelCreated {
                        cid: hdr.cid,
                        sid: hdr.available,
                        data_type: hdr.data_type,
                        element_count: hdr.actual_count(),
                        access,
                        server_addr,
                        priority,
                    });
                }
                CA_PROTO_READ_NOTIFY => {
                    // Direct dispatch to the in-flight read registry
                    // (Option C Phase A) — bypasses the coordinator's
                    // `tokio::select!` loop. Plain scalar reads are
                    // decoded here so the hot path does not allocate
                    // one payload Vec per response.
                    let ioid = hdr.available;
                    if let Some(subid) = in_flight.take_sub_update(ioid) {
                        // Circuit-recovery re-subscribe (C
                        // `tcpiiu::subscriptionUpdateRequest`): the reply is
                        // the subscription's post-recovery value, not a get
                        // result — route it to the monitor callback exactly
                        // as C does by issuing the request under the
                        // subscription's own id.
                        let _ = event_tx.send(if hdr.cid == ECA_NORMAL {
                            TransportEvent::MonitorData {
                                subid,
                                data_type: hdr.data_type,
                                count: hdr.actual_count(),
                                data: accumulated[data_start..data_start + actual_post].to_vec(),
                            }
                        } else {
                            TransportEvent::MonitorStatusError {
                                subid,
                                eca_status: hdr.cid,
                            }
                        });
                    } else if hdr.cid == ECA_NORMAL {
                        let data = &accumulated[data_start..data_start + actual_post];
                        dispatch_read_reply_with(&in_flight, ioid, |mode| {
                            make_read_reply(mode, hdr.data_type, hdr.actual_count(), data)
                        });
                    } else {
                        // libca `cac::readNotifyRespAction`
                        // (`cac.cpp`) calls
                        // `pmiu->exception(hdr.m_cid, "read failed", …)`,
                        // propagating the server's exact ECA code (the C
                        // server stamps `m_cid = ECA_GETFAIL` on a GET
                        // failure via `cas_set_header_cid`). Carry that
                        // raw code through `ServerError` — matching the
                        // sibling CA_PROTO_ERROR read path (below) and the
                        // EVENT_ADD `MonitorStatusError` path. Wrapping it
                        // in `Protocol` would lose the code: `Protocol(_)
                        // .to_eca_status()` falls to `ECA_PUTFAIL`, so a
                        // GET failure would surface as a *put* error.
                        dispatch_read_error(
                            &in_flight,
                            ioid,
                            epics_base_rs::error::CaError::ServerError(hdr.cid),
                        );
                    }
                }
                CA_PROTO_WRITE_NOTIFY => {
                    // Direct dispatch to the in-flight write registry
                    // (Option C Phase A). Mirrors the read path: the
                    // originating `ch.put()` task is awaiting the
                    // oneshot we resolve here. `hdr.cid` carries the
                    // ECA status — `1` (`ECA_NORMAL`) means success;
                    // anything else is mapped to `CaError::WriteFailed`.
                    let ioid = hdr.available;
                    let status = hdr.cid;
                    if let Some((_, (_, reply_tx))) = in_flight.writes.remove(&ioid) {
                        if status == 1 || status == ECA_NORMAL {
                            let _ = reply_tx.send(Ok(()));
                        } else {
                            let _ = reply_tx
                                .send(Err(epics_base_rs::error::CaError::WriteFailed(status)));
                        }
                    }
                }
                CA_PROTO_EVENT_ADD => {
                    // libca `cac::eventAddRespAction` (`cac.cpp:960`)
                    // gates the data delivery on `hdr.m_cid ==
                    // ECA_NORMAL`. The CA server uses non-NORMAL m_cid
                    // values on monitor frames to deliver out-of-band
                    // status to the subscriber — specifically
                    // `rsrv/camessage.c::no_read_access_event` emits
                    // ECA_NORDACCESS with a zeroed payload of full
                    // DBR size when read access for an active
                    // subscription is denied (e.g. after an ACF
                    // reload that revokes the client's identity).
                    // Without the gate, Rust would parse the zeroed
                    // payload as legitimate data and surface
                    // `value = 0` to the subscriber — silent
                    // "successful read of zero" instead of an access
                    // denial.
                    //
                    // The Rust SERVER tears down subscriptions on
                    // NoAccess, so Rust ↔ Rust never hits
                    // this path. But Rust client ↔ C IOC does — C IOC
                    // delivers the no-read-access frame instead of
                    // tearing down. Gate matches libca.
                    //
                    // C `libca/cac.cpp::eventRespAction()`
                    // returns immediately when `!hdr.m_postsize`,
                    // BEFORE the status/payload handling. Rsrv's
                    // `event_cancel_reply` intentionally sends a
                    // zero-payload `CA_PROTO_EVENT_ADD` confirmation;
                    // treating it as monitor data or as a status
                    // error surfaces the cancel ack as a bogus
                    // monitor event in the rare race where the
                    // subscription record is still present. The
                    // `if/else if/else` chain below skips the entire
                    // monitor delivery path when this is set — the
                    // outer `offset += msg_len` still advances.
                    if actual_post == 0 {
                        // zero-payload EVENT_ADD = cancel ack, drop silently
                    } else if hdr.cid != ECA_NORMAL {
                        // libca `cac::eventAddRespAction`
                        // (`cac.cpp:973-977`): when the monitor frame
                        // carries a non-NORMAL status, drop the
                        // (zeroed) payload but route the status
                        // through the per-subscription exception
                        // callback. Pre-fix Rust just warn+dropped,
                        // so e.g. an ECA_NORDACCESS from a C IOC's
                        // `no_read_access_event` (sent when an ACF
                        // reload revoked read access on an active
                        // subscription) was invisible to the
                        // subscriber. The bogus zeroed payload is
                        // still discarded — only the status is
                        // delivered — because libca only invokes
                        // `pmiu->exception(status)`, never the
                        // completion callback, on this path.
                        tracing::warn!(
                            server = %server_addr,
                            subid = hdr.available,
                            status = hdr.cid,
                            "MONITOR status error (libca: routes through subscription exception callback)"
                        );
                        metrics::counter!("ca_client_monitor_status_drops_total").increment(1);
                        let _ = event_tx.send(TransportEvent::MonitorStatusError {
                            subid: hdr.available,
                            eca_status: hdr.cid,
                        });
                    } else {
                        let data = accumulated[data_start..data_start + actual_post].to_vec();
                        let _ = event_tx.send(TransportEvent::MonitorData {
                            subid: hdr.available,
                            data_type: hdr.data_type,
                            count: hdr.actual_count(),
                            data,
                        });
                    }
                }
                CA_PROTO_ECHO | CA_PROTO_READ_SYNC => {
                    // Echo response from server — liveness already handled
                    // above (echo_pending=false).  Do NOT echo back; only
                    // the server echoes requests.  Responding here would
                    // create a tight ping-pong loop.
                }
                CA_PROTO_CREATE_CH_FAIL => {
                    let _ = event_tx.send(TransportEvent::ChannelCreateFailed { cid: hdr.cid });
                }
                CA_PROTO_ERROR => {
                    // CA_PROTO_ERROR wire layout per C `vsend_err`
                    // (`rsrv/camessage.c:139-224`):
                    //   resp.m_cid       = channel cid (or
                    //                      0xFFFFFFFF for non-channel-
                    //                      scoped commands like SEARCH
                    //                      or unknown-cmd reject)
                    //   resp.m_available = ECA status code (caerr.h)
                    //   payload          = original 16-byte header copy
                    //                      + NUL-terminated diag msg
                    //
                    // libca `cac::exceptionRespAction`
                    // (`modules/ca/src/client/cac.cpp:1118`) passes
                    // `hdr.m_available` as the status to the per-cmd
                    // exception stub — `m_available` is authoritative.
                    //
                    // Commit 21240ad fixed the same field-swap on the
                    // Rust SERVER side; this round closes it on the
                    // Rust CLIENT side. Pre-fix Rust read `hdr.cid` as
                    // the ECA status, so a CA_PROTO_ERROR from a C IOC
                    // surfaced the channel cid as the user-facing
                    // `CaException.status` — the actual ECA code (and
                    // therefore the entire exception-callback contract)
                    // was wrong. Symptom: clients can't distinguish
                    // ECA_BADTYPE from ECA_NORDACCESS etc.
                    let eca_status = hdr.available;
                    let orig_cmd = if actual_post >= 16 {
                        let orig_hdr_bytes = &accumulated[data_start..data_start + 16];
                        Some(u16::from_be_bytes([orig_hdr_bytes[0], orig_hdr_bytes[1]]))
                    } else {
                        None
                    };
                    // C `cac::exceptionRespAction` (`cac.cpp:1097-1107`):
                    // when the echoed request used the extended layout
                    // (`m_postsize == 0xffff`), the 8-byte annex carrying
                    // the full 32-bit postsize and count sits between the
                    // 16-byte header echo and the diagnostic string.
                    // Pre-fix Rust unconditionally started the diag at
                    // `data_start + 16`, so an extended READ/WRITE error
                    // surfaced the annex bytes as the leading 8 bytes of
                    // `msg` (mis-framed for the caller) and any actual
                    // diag string was truncated by 8 bytes.
                    let echo_hdr_size = if actual_post >= 16
                        && u16::from_be_bytes([
                            accumulated[data_start + 2],
                            accumulated[data_start + 3],
                        ]) == 0xFFFF
                        && actual_post >= 24
                    {
                        24
                    } else {
                        16
                    };
                    let msg = if actual_post > echo_hdr_size {
                        let msg_bytes =
                            &accumulated[data_start + echo_hdr_size..data_start + actual_post];
                        let end = msg_bytes
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(msg_bytes.len());
                        String::from_utf8_lossy(&msg_bytes[..end]).to_string()
                    } else {
                        String::new()
                    };
                    // route to the in-flight operation registry
                    // matching the echoed request command. libca
                    // `cac::exceptionRespAction` (`cac.cpp:1081-1119`)
                    // dispatches by original command through
                    // `tcpExcepJumpTableCAC`; readNotifyExcep /
                    // writeNotifyExcep use `hdr.m_available` to
                    // complete and uninstall the pending IO callback
                    // so the user-facing `get()` / `put()` future
                    // surfaces the per-op error instead of timing
                    // out. Pre-fix Rust only fired the global
                    // exception hook here, leaving the per-op
                    // futures pending until their own timeout.
                    // Extract `m_available` from the echoed 16-byte
                    // header (offset 12) — the extended annex
                    // (postsize/count fields) is appended AFTER the
                    // 16-byte header, so `m_available` is always at
                    // the same position regardless of extended form.
                    let echo_available = if actual_post >= 16 {
                        Some(u32::from_be_bytes([
                            accumulated[data_start + 12],
                            accumulated[data_start + 13],
                            accumulated[data_start + 14],
                            accumulated[data_start + 15],
                        ]))
                    } else {
                        None
                    };
                    if let (Some(cmd), Some(ioid)) = (orig_cmd, echo_available) {
                        match cmd {
                            CA_PROTO_READ_NOTIFY => {
                                dispatch_read_error(
                                    &in_flight,
                                    ioid,
                                    epics_base_rs::error::CaError::ServerError(eca_status),
                                );
                            }
                            CA_PROTO_WRITE_NOTIFY => {
                                if let Some((_, (_, reply_tx))) = in_flight.writes.remove(&ioid) {
                                    let _ = reply_tx.send(Err(
                                        epics_base_rs::error::CaError::ServerError(eca_status),
                                    ));
                                }
                            }
                            CA_PROTO_EVENT_ADD => {
                                // C `cac::eventAddExcep` (`cac.cpp:1030-1038`,
                                // jump-table entry at `cac.cpp:97`) routes the
                                // echoed EVENT_ADD's `m_available` (the
                                // subscription id) through
                                // `ioExceptionNotify` — the status reaches the
                                // subscription's exception callback and the
                                // subscription STAYS INSTALLED. Read/write use
                                // `ioExceptionNotifyAndUninstall` instead; the
                                // asymmetry is deliberate, because rsrv keeps
                                // re-posting the monitor (`camessage.c:513-522`
                                // emits this ERROR on every send-buffer-load
                                // failure while the circuit stays up), so the
                                // subscription must survive to receive the next
                                // attempt.
                                //
                                // `on_monitor_error` (via MonitorStatusError)
                                // is exactly that: delivers
                                // `Err(ServerError(status))` to the subscriber
                                // without removing the registry record.
                                let _ = event_tx.send(TransportEvent::MonitorStatusError {
                                    subid: ioid,
                                    eca_status,
                                });
                            }
                            // EVENT_CANCEL confirmations have no per-op waiter;
                            // C maps them to `defaultExcep` (global exception
                            // hook only) — the `ServerError` event below.
                            _ => {}
                        }
                    }
                    // C ref: modules/ca/src/client/udpiiu.cpp:exceptionRespAction —
                    // commit a352865 routes the error prefix through ERL_ERROR
                    // (ANSI-colored "Error:" on TTYs). The Rust equivalent is
                    // tracing::error! which honors the configured subscriber's
                    // formatting (color, prefix, structured fields).
                    tracing::error!(
                        server = %server_addr,
                        eca = eca_status,
                        cmd = ?orig_cmd,
                        msg = %msg,
                        "CA server error",
                    );
                    // The echoed request's DBR type and element count —
                    // libca's `caHdrLargeArray.m_dataType` / `m_count`, which
                    // every exception stub forwards and the default handler
                    // prints (`type=DBR_STRING, count=1`). The extended form
                    // (`m_count == 0`, postsize `0xFFFF`) carries the real
                    // count in the 8-byte annex, same place `echo_hdr_size`
                    // already accounts for.
                    let (echo_type, echo_count) = if actual_post >= 16 {
                        let e = &accumulated[data_start..data_start + 16];
                        let ty = u16::from_be_bytes([e[4], e[5]]);
                        let short_count = u16::from_be_bytes([e[6], e[7]]);
                        let count = if echo_hdr_size == 24 {
                            let a = &accumulated[data_start + 16..data_start + 24];
                            u32::from_be_bytes([a[4], a[5], a[6], a[7]])
                        } else {
                            u32::from(short_count)
                        };
                        (Some(ty), Some(count))
                    } else {
                        (None, None)
                    };
                    // rsrv stamps the outer `m_cid` with the client's cid for
                    // channel-scoped commands and `0xFFFF_FFFF` otherwise
                    // (`rsrv/camessage.c:155-182`).
                    let err_cid = (hdr.cid != 0xFFFF_FFFF).then_some(hdr.cid);
                    let _ = event_tx.send(TransportEvent::ServerError {
                        eca_status,
                        original_request: orig_cmd,
                        message: msg,
                        server_addr,
                        cid: err_cid,
                        data_type: echo_type,
                        count: echo_count,
                    });
                }
                CA_PROTO_SERVER_DISCONN => {
                    // server retired this cid — drop it from
                    // the post-create set so a same-cid CREATE_CHAN
                    // reuse later in the circuit starts fresh.
                    known_cids.remove(&hdr.cid);
                    pending_access.remove(&hdr.cid);
                    let _ = event_tx.send(TransportEvent::ServerDisconnect {
                        cid: hdr.cid,
                        server_addr,
                    });
                }
                // opcodes that C `libca/cac.cpp:60-89`
                // dispatches through its TCP jump table but Rust
                // didn't have a per-opcode arm for. Rust once made
                // unknown opcodes lethal (close circuit), so
                // benign frames from a gateway / name-server
                // / legacy IOC ended up tearing the Rust circuit
                // down on every occurrence. Accept them here as
                // no-ops:
                //   * CA_PROTO_SEARCH (6) — used when a CA
                //     server doubles as a name server
                //     (EPICS_CA_NAME_SERVERS); libca routes via
                //     `tcpiiu::searchRespNotify` and our TCP
                //     search path already has a separate
                //     nameserver pipeline.
                //   * CA_PROTO_READ (3) — deprecated synchronous
                //     read response; libca handles via
                //     `cac::readRespAction`. Rust never sends
                //     CA_PROTO_READ (only READ_NOTIFY), so any
                //     reply on this opcode is informational.
                //   * CA_PROTO_CLEAR_CHANNEL (12) — `cac.cpp:
                //     1000-1003` `clearChannelRespAction` is
                //     currently a documented no-op in C.
                CA_PROTO_SEARCH | CA_PROTO_READ | CA_PROTO_CLEAR_CHANNEL => {
                    tracing::trace!(
                        server = %server_addr,
                        cmd = hdr.cmmd,
                        "TCP no-op opcode received (libca-recognised, Rust ignores)"
                    );
                }
                unknown => {
                    // C `libca/cac.cpp::executeResponse()`
                    // dispatches unknown opcodes to
                    // `badTCPRespAction()`, which logs and returns
                    // false; `tcpiiu.cpp` treats
                    // `processIncoming() == false` as a protocol
                    // failure and calls `initiateAbortShutdown()`.
                    // Pre-fix Rust skipped unknown opcodes
                    // silently — a broken or hostile server could
                    // inject response frames that libca uses to
                    // tear down the circuit while Rust quietly
                    // advanced past them. Emit TcpClosed so the
                    // coordinator drops the circuit; the
                    // surrounding reconnect path will rebuild.
                    tracing::warn!(
                        server = %server_addr,
                        cmd = unknown,
                        "unknown TCP response opcode; closing circuit (C badTCPRespAction parity)"
                    );
                    metrics::counter!("ca_client_bad_tcp_response_total").increment(1);
                    let _ = event_tx.send(TransportEvent::TcpClosed {
                        server_addr,
                        priority,
                    });
                    return;
                }
            }

            offset += msg_len;
        }

        if offset > 0 {
            accumulated.drain(..offset);
        }

        // libca flow control, once per received frame, after the frame's
        // messages have been processed (C `tcpiiu.cpp:543-572`).
        //
        // Key on the OS socket buffer, never on how far behind the
        // application is. A consumer that stops polling its `MonitorHandle`
        // must not be able to hold `EVENTS_OFF` down for every *other*
        // subscription on the circuit — and it cannot, because the moment
        // this socket reads clean, flow control lifts.
        let want_flow_control = if bytes_pending_in_os() {
            if !flow_control_active {
                contig_recv_msg_count = contig_recv_msg_count.saturating_add(1);
            }
            contig_recv_msg_count >= max_contig_frames
        } else {
            // "if no bytes are pending then we must immediately switch off
            // flow control w/o waiting for more data to arrive"
            // (`tcpiiu.cpp:559-561`).
            contig_recv_msg_count = 0;
            false
        };
        if want_flow_control != flow_control_active {
            let cmd = if want_flow_control {
                CA_PROTO_EVENTS_OFF
            } else {
                CA_PROTO_EVENTS_ON
            };
            if write_tx
                .send(CaHeader::new(cmd).to_bytes().to_vec())
                .is_err()
            {
                let _ = event_tx.send(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                });
                return;
            }
            flow_control_active = want_flow_control;
        }
    }
}

// Host/tokio-only, and for one reason: these are **virtual-time** tests. They
// compress 30-50 s of watchdog arithmetic into microseconds with
// `#[tokio::test(start_paused = true)]`, which advances *tokio's* clock. The
// circuit path now takes its deadlines and its timeouts from the
// `runtime::task` seam (it has to — the RTEMS target has no tokio reactor for
// them to run on, doc/calink-rtems-design.md §11.1), and under
// `rtems-exec-model` that seam is the delayed-callback timer on the real
// `std::time` clock, which `start_paused` cannot move. So under that feature
// these would wait out the wall clock rather than test anything, in the same
// way `server_connection_drop_tests` below is inapplicable there.
//
// What they cover — the deadline arithmetic itself — is backend-independent
// and is covered in the default configuration, which is where they run.
#[cfg(all(test, not(feature = "rtems-exec-model")))]
mod read_loop_tests {
    //! Virtual-time tests for the libca-style lazy-echo watchdog.
    //!
    //! `tokio::test(start_paused = true)` gives us a paused clock
    //! that auto-advances whenever all tasks are pending on time.
    //! That makes the deadline arithmetic deterministic: we can
    //! sleep the test thread to a specific virtual instant, inject
    //! beacon-arrival or data events, and assert what the read loop
    //! has produced by that point — without actual wall-clock
    //! waits that would make the test suite slow and flaky.
    //!
    //! All three tests assume the default `EPICS_CA_CONN_TMO` of 30
    //! seconds (echo_idle_secs). Tests do not set the env var to
    //! avoid cross-test contamination.
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// Spin up a read loop wired to a duplex pipe (so the test can
    /// drive the "server" end), an event channel, a frame channel
    /// (where the read loop's outgoing echo requests land), and a
    /// beacon-arrival channel. Returns the handles the test needs.
    fn spawn_read_loop() -> (
        tokio::io::DuplexStream,                 // server end of pipe
        mpsc::UnboundedReceiver<TransportEvent>, // events emitted
        mpsc::UnboundedReceiver<Vec<u8>>,        // frames the loop wrote
        mpsc::UnboundedSender<bool>,             // beacon arrival sender
        tokio::task::JoinHandle<()>,             // the loop task
    ) {
        let (server_end, client_end) = tokio::io::duplex(8192);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let (beacon_tx, beacon_rx) = mpsc::unbounded_channel::<bool>();
        let task = tokio::spawn(read_loop(
            client_end,
            test_addr(),
            0,
            event_tx,
            write_tx,
            beacon_rx,
            crate::client::types::InFlightOps::new(),
            std::sync::Arc::new(dashmap::DashMap::new()),
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));
        (server_end, event_rx, write_rx, beacon_tx, task)
    }

    /// Healthy beacon arriving partway through the idle window
    /// pushes the deadline forward (libca `beaconArrivalNotify`).
    /// Without the refresh, the loop would echo at t=30 s; with
    /// the refresh at t=20 s, the new deadline is t=50 s and no
    /// echo fires before then.
    #[tokio::test(start_paused = true)]
    async fn healthy_beacon_extends_idle_deadline() {
        let (_server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();

        // Yield once so the spawned read_loop is actually running
        // before we start manipulating time. Without this, the
        // first `sleep` below races the spawn.
        tokio::task::yield_now().await;

        // Advance to t=20 s and push a healthy beacon. Idle
        // deadline was 30 s; after the beacon it becomes 50 s.
        tokio::time::sleep(Duration::from_secs(20)).await;
        beacon_tx.send(false).expect("beacon channel alive");

        // Advance to t=45 s (still under the new 50-s deadline).
        // No echo must have fired yet.
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(
            writes.try_recv().is_err(),
            "healthy beacon should have extended the idle deadline past 30 s"
        );

        // Advance past t=50 s. Now the (refreshed) idle deadline
        // has expired and the loop sent an echo.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let frame = writes
            .try_recv()
            .expect("echo must fire after extended deadline");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE,
            "idle echo must be a bare CA header"
        );

        task.abort();
        let _ = events.try_recv();
    }

    /// Anomaly beacon sets a sticky flag (libca
    /// `beaconAnomalyNotify`); subsequent healthy beacons must
    /// NOT refresh the deadline while the flag is set. Result:
    /// the watchdog expires on its original 30-s schedule even
    /// though healthy beacons kept arriving.
    #[tokio::test(start_paused = true)]
    async fn anomaly_beacon_suppresses_healthy_refresh() {
        let (_server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();
        tokio::task::yield_now().await;

        // Anomaly at t=5 s — flag set, deadline UNCHANGED at 30 s.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(true).expect("alive");

        // Spurious healthy beacons at t=10, t=20 — must not
        // refresh because the flag is sticky.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(false).expect("alive");
        tokio::time::sleep(Duration::from_secs(10)).await;
        beacon_tx.send(false).expect("alive");

        // Advance to t=31 s — past the original 30-s deadline.
        // Echo must have fired exactly because the flag prevented
        // any refresh. (Ordering of the previous beacon sends:
        // they're all consumed before time advances past 30 s,
        // because tokio polls tasks until pending before advancing.)
        tokio::time::sleep(Duration::from_secs(11)).await;
        let frame = writes
            .try_recv()
            .expect("anomaly flag must let watchdog expire on original schedule");
        assert_eq!(frame.len(), CaHeader::SIZE);

        task.abort();
        let _ = events.try_recv();
    }

    /// Data arrival from the server (libca `messageArrivalNotify`)
    /// clears both `echo_pending` and `beacon_anomaly`, and
    /// refreshes the deadline. After clearing, healthy beacons can
    /// once again refresh.
    #[tokio::test(start_paused = true)]
    async fn data_arrival_clears_anomaly_flag_and_resumes_refresh() {
        let (mut server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();
        tokio::task::yield_now().await;

        // Anomaly at t=5 s.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(true).expect("alive");

        // Server sends a CA_PROTO_VERSION frame at t=10 s. This
        // is real data → flag clears, deadline pushed to 10+30=40.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
        version_hdr.count = 13; // some minor version
        server_end
            .write_all(&version_hdr.to_bytes())
            .await
            .expect("server end write");

        // Confirm read_loop picked up the version event. This is
        // also the moment the flag clears.
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("ServerVersion within 1 s")
            .expect("not closed");
        match event {
            TransportEvent::ServerVersion { minor_version, .. } => {
                assert_eq!(minor_version, 13);
            }
            _ => panic!("expected ServerVersion event"),
        }

        // Healthy beacon at t=15 s — flag is cleared so this
        // refreshes the deadline to 15+30=45.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(false).expect("alive");

        // Advance to t=42 s (still under 45). No echo yet.
        tokio::time::sleep(Duration::from_secs(27)).await;
        assert!(
            writes.try_recv().is_err(),
            "post-data-arrival healthy beacon must refresh the deadline"
        );

        // Advance to t=46 s — past the refreshed deadline.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let frame = writes
            .try_recv()
            .expect("echo fires once the refreshed deadline expires");
        assert_eq!(frame.len(), CaHeader::SIZE);

        task.abort();
    }
}

// Host/tokio-only: constructs `ServerConnection` tasks with `tokio::spawn` and
// asserts tokio abort semantics. Under `rtems-exec-model` the task fields are
// `JoinFuture` (not tokio handles) and the async client stack has no reactor to
// run on, so this test is inapplicable there.
#[cfg(all(test, not(feature = "rtems-exec-model")))]
mod server_connection_drop_tests {
    //! Verifies the per-circuit `ServerConnection::drop` aborts both
    //! its read and write tasks. Without this, every `connections`
    //! HashMap drop path (send-buffer-stall removal, transport
    //! manager exit, `CaClient::drop`) would detach the JoinHandles
    //! and leave the spawned per-server tasks running until process
    //! exit. The companion `CaClient::Drop` only aborts top-level
    //! tasks (coordinator / search / transport / beacon); this
    //! per-connection Drop is what makes the cascade complete.
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn drop_aborts_read_and_write_tasks() {
        // Long-running dummy tasks that never complete on their own.
        let read_task = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let write_task = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        // AbortHandle sticks around after the JoinHandle is moved
        // into ServerConnection — lets us observe the post-drop
        // task state.
        let read_abort = read_task.abort_handle();
        let write_abort = write_task.abort_handle();

        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        #[cfg(ca_beacon_monitor)]
        let (beacon_arrival_tx, _ba_rx) = mpsc::unbounded_channel::<bool>();

        let conn = ServerConnection {
            server_minor: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
            write_tx,
            pending_frames: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(ca_beacon_monitor)]
            beacon_arrival_tx,
            _read_task: read_task,
            _write_task: write_task,
        };

        // Pre-drop: tasks are still running.
        assert!(!read_abort.is_finished());
        assert!(!write_abort.is_finished());

        drop(conn);

        // tokio's abort schedules cancellation; let the runtime
        // drain it.
        let drain_started = tokio::time::Instant::now();
        for _ in 0..50 {
            if read_abort.is_finished() && write_abort.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let drain_elapsed = drain_started.elapsed();

        assert!(
            read_abort.is_finished(),
            "ServerConnection::drop must abort _read_task"
        );
        assert!(
            write_abort.is_finished(),
            "ServerConnection::drop must abort _write_task"
        );

        // Reproducer guard for epics-base issue #477 (30s hang after
        // both ends are destroyed): if Drop ever stops aborting the
        // pumps, the test would loop the full 50 × 2 ms = 100 ms
        // budget then fail above. Tighten the budget here so a
        // regression toward "let echo timeout drain" (which would
        // approach the upstream 30 s symptom) shows up immediately.
        assert!(
            drain_elapsed < Duration::from_millis(500),
            "abort cascade took {drain_elapsed:?} — far over the \
             tens-of-milliseconds budget (#477 reproducer)"
        );
    }
}

#[cfg(test)]
mod recv_body_limit_tests {
    //! R6-21: the receive path must accept any payload a C server can send.
    //! C bounds the receive body with `EPICS_CA_AUTO_ARRAY_BYTES` (default
    //! YES ⇒ NO bound, `cac.cpp:227-232`), not `EPICS_CA_MAX_ARRAY_BYTES`,
    //! and an over-bound response is IGNORED, never fatal
    //! (`tcpiiu.cpp:1269-1283`). The pre-fix Rust client closed the circuit
    //! when the accumulated buffer passed a cap derived from
    //! `EPICS_CA_MAX_ARRAY_BYTES` — permanently, since the server re-sends
    //! the same waveform on reconnect.
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// A frame announcing a body far larger than `max_frame_body_bytes()` must
    /// NOT close the circuit: the loop waits for the body, exactly as C waits
    /// for the bytes in `recvQue`. (Pre-fix: `TcpClosed` immediately, both
    /// from the parser's "payload too large" and from the accumulation cap.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversize_body_announcement_does_not_close_the_circuit() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // Extended header announcing 3x max_frame_body_bytes() of body.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        let huge = (crate::protocol::max_frame_body_bytes() * 3) as u32;
        frame.extend_from_slice(&huge.to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(frame.len(), 24);

        let mut client = client_io;
        client.write_all(&frame).await.expect("write header");
        client.flush().await.expect("flush");

        let early = tokio::time::timeout(Duration::from_millis(300), event_rx.recv()).await;
        assert!(
            early.is_err(),
            "an oversize payload must never close a CA circuit — C logs and \
             ignores the message, and by default has no size limit at all"
        );

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }

    /// The end of the same story: a body PAST `max_frame_body_bytes()` is
    /// delivered, and framing resumes on the frame that follows it. Uses a
    /// 20 MiB payload — over the 16 MiB `max_frame_body_bytes()` default that the
    /// pre-fix client turned into a hard close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn body_larger_than_max_frame_body_bytes_is_delivered() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // 20 MiB EVENT_ADD body for a subscription nobody is waiting on: the
        // dispatcher drops it, but the framer must consume exactly
        // 24 + 20 MiB bytes and carry on.
        const BODY: usize = 20 * 1024 * 1024;
        assert!(BODY > crate::protocol::max_frame_body_bytes() / 2);
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        frame.extend_from_slice(&(BODY as u32).to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.resize(24 + BODY, 0);

        // Trailing VERSION frame: proves the framer resynchronised on the
        // byte after the huge body rather than closing or desyncing.
        let mut version = CaHeader::new(CA_PROTO_VERSION);
        version.count = 13;
        frame.extend_from_slice(&version.to_bytes());

        let mut client = client_io;
        tokio::spawn(async move {
            let _ = client.write_all(&frame).await;
            let _ = client.flush().await;
            // Keep the write half alive until the loop has parsed.
            tokio::time::sleep(Duration::from_secs(3)).await;
        });

        let evt = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(TransportEvent::ServerVersion { minor_version, .. }) => {
                        return Some(minor_version);
                    }
                    Some(TransportEvent::TcpClosed { .. }) => return None,
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("read_loop must parse past a 20 MiB body");
        assert_eq!(
            evt,
            Some(13),
            "the frame after a 20 MiB body must still be framed — a large \
             array must not close or desync the circuit"
        );

        loop_handle.abort();
    }
}

#[cfg(test)]
mod malformed_header_close_tests {
    //! BUG 3: the client read loop must distinguish a *partial*
    //! extended header (await more bytes) from a *definitively
    //! malformed* one (close the connection). A blanket "await more"
    //! spins forever re-parsing the same bad bytes.
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn loop_inputs() -> (
        SocketAddr,
        mpsc::UnboundedReceiver<TransportEvent>,
        mpsc::UnboundedSender<TransportEvent>,
        mpsc::UnboundedSender<Vec<u8>>,
        mpsc::UnboundedReceiver<bool>,
        super::super::types::InFlightOps,
        super::super::types::ServerLastRxAt,
    ) {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        (
            server_addr,
            event_rx,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
        )
    }

    /// A definitively malformed header must CLOSE the connection, and after
    /// R6-21 the definitive malformation is C's: a payload size that is not
    /// 8-byte aligned (`tcpiiu.cpp:1202-1207`, "server sent missaligned
    /// payload" ⇒ `return false` ⇒ circuit dropped). A merely *large*
    /// postsize is NOT malformed — see `recv_body_limit_tests`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn misaligned_payload_closes_connection() {
        let (server_addr, mut event_rx, event_tx, write_tx, ba_rx, in_flight, last_rx_at) =
            loop_inputs();
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // 16-byte header declaring a 12-byte payload: 12 & 0x7 != 0.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 12;
        let frame = hdr.to_bytes().to_vec();

        let mut client = client_io;
        client.write_all(&frame).await.expect("write bad header");
        client.flush().await.expect("flush");

        // The read loop must close — emit TcpClosed and return — WITHOUT
        // waiting for more bytes. We keep the write half open so this
        // only passes if the loop closes on its own.
        let closed = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("read_loop must close on a misaligned payload, not spin");
        assert!(
            matches!(closed, Some(TransportEvent::TcpClosed { .. })),
            "misaligned payload must emit TcpClosed"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
        drop(client);
    }

    /// Control: a *partial* extended header (only 20 of 24 bytes) must
    /// NOT close — `read_loop` waits for the remaining bytes. Closing
    /// here would be a false-positive disconnect on a benign TCP
    /// segment boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_extended_header_waits_not_closes() {
        let (server_addr, mut event_rx, event_tx, write_tx, ba_rx, in_flight, last_rx_at) =
            loop_inputs();
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // 20 bytes: 16-byte base header with postsize=0xFFFF + only 4 of
        // the 8 extended bytes.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        frame.extend_from_slice(&[0u8, 0, 0, 0]);
        assert_eq!(frame.len(), 20);

        let mut client = client_io;
        client.write_all(&frame).await.expect("write partial");
        client.flush().await.expect("flush");

        // No TcpClosed within 300ms — the loop is blocked awaiting bytes.
        let early = tokio::time::timeout(Duration::from_millis(300), event_rx.recv()).await;
        assert!(
            early.is_err(),
            "partial extended header must NOT close — read_loop waits \
             for the rest of the header"
        );

        // Clean EOF resolves the loop.
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }
}

#[cfg(test)]
mod error_echo_dispatch_tests {
    //! R8-16: a `CA_PROTO_ERROR` echoing an `EVENT_ADD` header must reach
    //! the subscription's callback. C `cac::exceptionRespAction` dispatches
    //! by the *echoed* command through `tcpExcepJumpTableCAC`
    //! (`cac.cpp:93-97`): EVENT_ADD → `eventAddExcep` → `ioExceptionNotify`
    //! (`cac.cpp:1030-1038`) — status delivered, subscription NOT
    //! uninstalled. rsrv emits this frame whenever a monitor update will
    //! not fit the send buffer (`camessage.c:513-522`) with the circuit
    //! staying up, so a swallowed error is a silently stalled monitor.
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Builds a `CA_PROTO_ERROR` frame whose 16-byte payload echoes a
    /// request header of `echo_cmd` carrying `echo_available`
    /// (ioid / subscription id).
    fn error_frame(echo_cmd: u16, echo_available: u32, eca_status: u32, cid: u32) -> Vec<u8> {
        let mut echoed = CaHeader::new(echo_cmd);
        echoed.postsize = 16;
        echoed.data_type = 6;
        echoed.count = 1;
        echoed.cid = 0x2A;
        echoed.available = echo_available;
        let echoed = echoed.to_bytes();

        let mut err = CaHeader::new(CA_PROTO_ERROR);
        err.postsize = echoed.len() as u16;
        err.cid = cid;
        err.available = eca_status;

        let mut frame = err.to_bytes().to_vec();
        frame.extend_from_slice(&echoed);
        frame
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn r8_16_error_echoing_event_add_reaches_the_subscription() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // ECA_TOLARGE echoing subscription id 0xCAFE_BABE — the exact
        // frame rsrv sends when a monitor update overruns the send buffer.
        const SUBID: u32 = 0xCAFE_BABE;
        const ECA_TOLARGE: u32 = 0xC8;
        let frame = error_frame(CA_PROTO_EVENT_ADD, SUBID, ECA_TOLARGE, 0x2A);

        let mut client = client_io;
        client.write_all(&frame).await.expect("write ERROR frame");
        client.flush().await.expect("flush");

        // The subscription-scoped delivery must appear. Pre-fix the
        // dispatcher's `_ => {}` arm swallowed it and only the global
        // `ServerError` hook fired.
        let mut saw_status_error = false;
        let mut saw_server_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && !(saw_status_error && saw_server_error) {
            match tokio::time::timeout_at(deadline, event_rx.recv()).await {
                Ok(Some(TransportEvent::MonitorStatusError { subid, eca_status })) => {
                    assert_eq!(subid, SUBID, "must carry the echoed subscription id");
                    assert_eq!(eca_status, ECA_TOLARGE, "must carry the ECA status");
                    saw_status_error = true;
                }
                Ok(Some(TransportEvent::ServerError { .. })) => saw_server_error = true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            saw_status_error,
            "CA_PROTO_ERROR echoing EVENT_ADD must be routed to the \
             subscription (C: eventAddExcep → ioExceptionNotify)"
        );
        assert!(
            saw_server_error,
            "the global exception hook must still fire (C: cac::exception)"
        );

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }

    /// The asymmetry C encodes in the jump table: read/write use
    /// `ioExceptionNotifyAndUninstall`, EVENT_ADD does not. A read error
    /// must therefore still complete-and-remove its in-flight IO, and must
    /// NOT emit a subscription-scoped error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn r8_16_error_echoing_read_notify_still_uninstalls_the_io() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(256);

        const IOID: u32 = 7;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        in_flight.reads.insert(
            IOID,
            super::super::types::ReadWaiter::OneShot {
                cid: 0x2A,
                mode: super::super::types::ReadReplyMode::Plain,
                reply_tx,
            },
        );

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight.clone(),
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        const ECA_GETFAIL: u32 = 0xC4;
        let frame = error_frame(CA_PROTO_READ_NOTIFY, IOID, ECA_GETFAIL, 0x2A);
        let mut client = client_io;
        client.write_all(&frame).await.expect("write ERROR frame");
        client.flush().await.expect("flush");

        let got = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("read waiter must be completed by the echoed READ_NOTIFY error")
            .expect("waiter must not be dropped without a reply");
        assert!(
            matches!(
                got,
                Err(epics_base_rs::error::CaError::ServerError(ECA_GETFAIL))
            ),
            "read error must carry the ECA status"
        );
        assert!(
            !in_flight.reads.contains_key(&IOID),
            "read exceptions uninstall the IO (C: ioExceptionNotifyAndUninstall)"
        );

        // No subscription-scoped delivery for a read echo.
        let mut saw_status_error = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await
        {
            if matches!(ev, TransportEvent::MonitorStatusError { .. }) {
                saw_status_error = true;
            }
        }
        assert!(
            !saw_status_error,
            "a READ_NOTIFY echo must not be routed to a subscription"
        );

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }
}

#[cfg(test)]
mod flow_control_tests {
    //! R6-17: CA flow control keys on OS socket-buffer occupancy, exactly like
    //! C `tcpRecvThread::run` (`tcpiiu.cpp:543-572`). `busyStateDetected` is
    //! set only after `maxContiguousFrames` consecutive receive frames each
    //! left bytes pending in the OS, and is cleared the *first* time the
    //! socket reads clean — "w/o waiting for more data to arrive". Consumer
    //! backlog is not an input, so a stalled reader can never latch
    //! `EVENTS_OFF` on for the circuit's other subscriptions.
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Drive `read_loop` with a scriptable occupancy probe and collect the
    /// flow-control frames it emits.
    struct Harness {
        client: tokio::io::DuplexStream,
        writes: mpsc::UnboundedReceiver<Vec<u8>>,
        busy: Arc<AtomicBool>,
        _events: mpsc::UnboundedReceiver<TransportEvent>,
        task: tokio::task::JoinHandle<()>,
    }

    fn harness() -> Harness {
        let (event_tx, _events) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, writes) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let busy = Arc::new(AtomicBool::new(false));
        let probe: OsRecvQueueProbe = {
            let busy = Arc::clone(&busy);
            Arc::new(move || busy.load(Ordering::SeqCst))
        };
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client, server_io) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(read_loop(
            server_io,
            "127.0.0.1:5064".parse().unwrap(),
            0,
            event_tx,
            write_tx,
            ba_rx,
            super::super::types::InFlightOps::new(),
            last_rx_at,
            Arc::new(UnresponsiveGate::new()),
            probe,
            Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));
        Harness {
            client,
            writes,
            busy,
            _events,
            task,
        }
    }

    /// Push one CA_PROTO_ECHO frame — a complete, harmless message — and wait
    /// for `read_loop` to consume it, so each call drives exactly one receive
    /// frame. Without the pause the duplex coalesces several frames into a
    /// single `read()`, which is one frame for contiguous-count purposes.
    async fn one_frame(h: &mut Harness) {
        let frame = CaHeader::new(CA_PROTO_ECHO).to_bytes().to_vec();
        h.client.write_all(&frame).await.expect("write");
        h.client.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    /// Drain whatever the loop queued for the wire, as command codes.
    async fn drain(h: &mut Harness) -> Vec<u16> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut out = Vec::new();
        while let Ok(frame) = h.writes.try_recv() {
            out.push(CaHeader::from_bytes(&frame).expect("header").cmmd);
        }
        out
    }

    /// The trigger is `max_contiguous_frames()` CONSECUTIVE busy frames —
    /// frame N-1 must not fire, frame N must. Then the very next drained read
    /// must lift it, with no consumer having drained anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn r6_17_events_off_at_the_contiguous_frame_boundary() {
        let trigger = crate::protocol::max_contiguous_frames();
        let mut h = harness();

        // Frames 1..trigger-1 with bytes still pending: under the bound, so no
        // flow control yet.
        h.busy.store(true, Ordering::SeqCst);
        for _ in 0..trigger - 1 {
            one_frame(&mut h).await;
        }
        assert_eq!(
            drain(&mut h).await,
            Vec::<u16>::new(),
            "{} contiguous busy frames is one short of the bound — no EVENTS_OFF",
            trigger - 1
        );

        // Frame `trigger`: the bound is reached.
        one_frame(&mut h).await;
        assert_eq!(
            drain(&mut h).await,
            vec![CA_PROTO_EVENTS_OFF],
            "the {trigger}th contiguous busy frame must trip EVENTS_OFF"
        );

        // Socket drains. No consumer did anything — libca lifts flow control
        // on the socket alone, immediately, on the very next frame.
        h.busy.store(false, Ordering::SeqCst);
        one_frame(&mut h).await;
        assert_eq!(
            drain(&mut h).await,
            vec![CA_PROTO_EVENTS_ON],
            "a drained socket must lift EVENTS_OFF immediately, with no \
             consumer-side drain"
        );

        h.task.abort();
    }

    /// A busy run BROKEN by one clean read resets the contiguous count to 0
    /// (C `contigRecvMsgCount = 0u`), so the next busy run starts over from 1
    /// rather than resuming near the bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn r6_17_a_clean_read_resets_the_contiguous_count() {
        let trigger = crate::protocol::max_contiguous_frames();
        let mut h = harness();

        h.busy.store(true, Ordering::SeqCst);
        for _ in 0..trigger - 1 {
            one_frame(&mut h).await;
        }
        // One clean read breaks the run.
        h.busy.store(false, Ordering::SeqCst);
        one_frame(&mut h).await;
        assert_eq!(drain(&mut h).await, Vec::<u16>::new());

        // Back to busy: a fresh run of trigger-1 must still be under the bound.
        h.busy.store(true, Ordering::SeqCst);
        for _ in 0..trigger - 1 {
            one_frame(&mut h).await;
        }
        assert_eq!(
            drain(&mut h).await,
            Vec::<u16>::new(),
            "the clean read must have reset the count — the run restarts at 1"
        );

        h.task.abort();
    }

    /// C `cac.cpp:233-237`: the trigger scales with EPICS_CA_MAX_ARRAY_BYTES,
    /// and at the C default one max-size array is exactly one receive buffer,
    /// so it stays at `contiguousMsgCountWhichTriggersFlowControl` = 10.
    #[test]
    fn r6_17_max_contiguous_frames_scales_from_max_array_bytes() {
        use crate::protocol::{
            COM_BUF_SIZE, MAX_TCP, max_array_bytes_buffer, max_contiguous_frames,
        };
        assert_eq!(
            MAX_TCP / COM_BUF_SIZE,
            1,
            "bufsPerArray is not > 1 at the C default"
        );
        assert!(
            max_array_bytes_buffer() >= MAX_TCP,
            "C rounds up to MAX_TCP"
        );
        assert!(
            max_contiguous_frames() >= 10,
            "never below C's contiguousMsgCountWhichTriggersFlowControl"
        );
    }
}

// Host/tokio-only, and for one reason: these are **virtual-time** tests. They
// compress 30-50 s of watchdog arithmetic into microseconds with
// `#[tokio::test(start_paused = true)]`, which advances *tokio's* clock. The
// circuit path now takes its deadlines and its timeouts from the
// `runtime::task` seam (it has to — the RTEMS target has no tokio reactor for
// them to run on, doc/calink-rtems-design.md §11.1), and under
// `rtems-exec-model` that seam is the delayed-callback timer on the real
// `std::time` clock, which `start_paused` cannot move. So under that feature
// these would wait out the wall clock rather than test anything, in the same
// way `server_connection_drop_tests` below is inapplicable there.
//
// What they cover — the deadline arithmetic itself — is backend-independent
// and is covered in the default configuration, which is where they run.
#[cfg(all(test, not(feature = "rtems-exec-model")))]
mod recv_watchdog_tests {
    //! R6-16: an echo-probe timeout on the receive watchdog must mark the
    //! circuit unresponsive and KEEP the socket. C `tcpRecvWatchdog::expire`
    //! returns `noRestart` after `receiveTimeoutNotify`
    //! (`tcpRecvWatchdog.cpp:54-81`); `tcpiiu::unresponsiveCircuitNotify`
    //! (`tcpiiu.cpp:899-941`) cancels both watchdogs, re-arms one more echo,
    //! raises `ECA_UNRESPTMO`, and never touches the socket. Only a genuine
    //! socket error tears the circuit down (`tcpiiu.cpp:586-601`).
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Idle expiry → echo probe → echo timeout must emit exactly one
    /// `CircuitUnresponsive`, a second echo, and NO `TcpClosed` — not even
    /// after many further echo-timeout windows elapse. Pre-fix the loop
    /// closed on the second echo timeout.
    #[tokio::test(start_paused = true)]
    async fn r6_16_echo_timeout_marks_unresponsive_and_keeps_socket() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let unresponsive = Arc::new(UnresponsiveGate::new());
        // A silent-but-open peer: the duplex never yields a byte, and we hold
        // `client_io` so the read never hits EOF.
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            super::super::types::InFlightOps::new(),
            last_rx_at,
            Arc::clone(&unresponsive),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        let idle = Duration::from_secs(echo_idle_secs());
        let echo = Duration::from_secs(ECHO_TIMEOUT_SECS);

        // Idle expiry: first echo probe departs, circuit still responsive.
        tokio::time::sleep(idle + Duration::from_millis(100)).await;
        let first = write_rx.try_recv().expect("idle expiry must send an echo");
        assert_eq!(
            CaHeader::from_bytes(&first).unwrap().cmmd,
            CA_PROTO_READ_SYNC,
            "idle expiry must emit the liveness probe (no VERSION frame was fed \
             in, so `server_minor_version` is 0 and `send_echo` picks the \
             pre-v4.3 CA_PROTO_READ_SYNC form)"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no event before the echo actually times out"
        );

        // Echo timeout: unresponsive + one trailing probe (C re-arms
        // `echoRequestPending` inside `unresponsiveCircuitNotify`).
        tokio::time::sleep(echo + Duration::from_millis(100)).await;
        assert!(
            matches!(
                event_rx.try_recv(),
                Ok(TransportEvent::CircuitUnresponsive { .. })
            ),
            "echo timeout must emit CircuitUnresponsive"
        );
        assert!(
            write_rx.try_recv().is_ok(),
            "unresponsiveCircuitNotify must re-arm one more echo request"
        );

        // The watchdog is now cancelled. Let ten more echo windows elapse:
        // no further probes, no further events, and above all no TcpClosed.
        tokio::time::sleep(echo * 10).await;
        assert!(
            write_rx.try_recv().is_err(),
            "a cancelled watchdog must not keep probing"
        );
        match event_rx.try_recv() {
            Err(_) => {}
            Ok(_) => panic!("unresponsive circuit must be kept, not torn down"),
        }
        assert!(
            !loop_handle.is_finished(),
            "read_loop must stay alive on an unresponsive circuit"
        );

        // Recovery: a byte from the server re-arms the watchdog and emits the
        // sole CircuitResponsive.
        let mut client = client_io;
        client
            .write_all(&CaHeader::new(CA_PROTO_ECHO).to_bytes())
            .await
            .expect("write echo reply");
        client.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            matches!(
                event_rx.try_recv(),
                Ok(TransportEvent::CircuitResponsive { .. })
            ),
            "a byte from the server must recover the circuit"
        );

        // Re-armed: the next idle window probes again.
        tokio::time::sleep(idle + Duration::from_millis(100)).await;
        assert!(
            write_rx.try_recv().is_ok(),
            "the watchdog must be re-armed after recovery"
        );

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }

    /// A genuine socket error (peer closes → EOF) is still the one thing that
    /// tears the circuit down (C `tcpiiu.cpp:586-601`).
    #[tokio::test(start_paused = true)]
    async fn r6_16_socket_eof_still_closes() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            super::super::types::InFlightOps::new(),
            last_rx_at,
            Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        drop(client_io);
        let closed = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("EOF must close promptly");
        assert!(
            matches!(closed, Some(TransportEvent::TcpClosed { .. })),
            "a real socket error must still emit TcpClosed"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }
    /// R18-18: the `READ_NOTIFY` the coordinator issues when a circuit becomes
    /// responsive again is C's `tcpiiu::subscriptionUpdateRequest`
    /// (`tcpiiu.cpp:1636-1641`), which puts the *subscription's* id in the
    /// request so the reply reaches the monitor callback. The port's `ioid`
    /// space is separate, so `InFlightOps::register_sub_update` records the
    /// owner; the reply must come out as `MonitorData`, not be dropped for
    /// want of a get waiter (pre-fix: nothing was ever emitted, so a
    /// `camonitor` never re-posted the value after the IOC un-hung).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_read_notify_reply_posts_to_the_subscription() {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        let (client_io, server_io) = tokio::io::duplex(1024);

        let ioid = in_flight.register_sub_update(42, 7);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight.clone(),
            last_rx_at,
            std::sync::Arc::new(UnresponsiveGate::new()),
            drained_socket_probe(),
            std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0)),
        ));

        // DBR_DOUBLE, one element: the post-recovery value.
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = 6;
        hdr.count = 1;
        hdr.postsize = 8;
        hdr.cid = crate::protocol::ECA_NORMAL;
        hdr.available = ioid;
        let mut frame = hdr.to_bytes().to_vec();
        frame.extend_from_slice(&3.5f64.to_be_bytes());

        let mut client = client_io;
        client.write_all(&frame).await.expect("write");
        client.flush().await.expect("flush");

        let evt = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("a recovery re-read must produce an event")
            .expect("channel open");
        match evt {
            TransportEvent::MonitorData {
                subid,
                data_type,
                count,
                data,
            } => {
                assert_eq!(subid, 7, "the reply belongs to the subscription");
                assert_eq!((data_type, count), (6, 1));
                assert_eq!(f64::from_be_bytes(data[..8].try_into().unwrap()), 3.5);
            }
            _ => panic!("the recovery re-read reply must be posted as MonitorData"),
        }
        assert!(
            in_flight.sub_updates.is_empty(),
            "the record is consumed by the reply"
        );

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }
}

// Host/tokio-only, and for one reason: these are **virtual-time** tests. They
// compress 30-50 s of watchdog arithmetic into microseconds with
// `#[tokio::test(start_paused = true)]`, which advances *tokio's* clock. The
// circuit path now takes its deadlines and its timeouts from the
// `runtime::task` seam (it has to — the RTEMS target has no tokio reactor for
// them to run on, doc/calink-rtems-design.md §11.1), and under
// `rtems-exec-model` that seam is the delayed-callback timer on the real
// `std::time` clock, which `start_paused` cannot move. So under that feature
// these would wait out the wall clock rather than test anything, in the same
// way `server_connection_drop_tests` below is inapplicable there.
//
// What they cover — the deadline arithmetic itself — is backend-independent
// and is covered in the default configuration, which is where they run.
#[cfg(all(test, not(feature = "rtems-exec-model")))]
mod write_loop_timeout_tests {
    //! R2-40: a send-side stall in `write_loop` must mark the circuit
    //! unresponsive (`CircuitUnresponsive`) and KEEP the socket, resuming
    //! the same batch when the peer drains — matching C `sendTimeoutNotify`
    //! → `unresponsiveCircuitNotify` (`tcpiiu.cpp:879-940`), which keeps
    //! the socket and echo-probes rather than tearing the circuit down.
    //! The stall-safety that previously forced a close (a cancelled
    //! `write_all` leaving a truncated frame with an unknown byte count)
    //! is gone: the loop drives the flush with `writer.write(&batch[n..])`
    //! and carries `written` across stalls, so a `Pending`-cancelled
    //! `write` (0 bytes per the `AsyncWrite` contract) resumes from the
    //! exact offset with no byte re-sent. A permanently dead circuit is
    //! closed by the read-side echo watchdog, not here.
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;

    /// Mock writer that accepts a 4-byte prefix on the first
    /// `poll_write` (a CA-frame prefix landing on the socket) and then
    /// stalls forever — every later `poll_write` returns `Pending`.
    struct PartialThenStallWriter {
        first_write: Arc<AtomicUsize>,
    }

    impl AsyncWrite for PartialThenStallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.first_write.swap(1, Ordering::SeqCst) == 0 {
                let n = buf.len().min(4);
                Poll::Ready(Ok(n))
            } else {
                Poll::Pending
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Mock writer that accepts a 4-byte prefix (poll 0), then stalls the
    /// remainder for one full send-watchdog window before accepting it.
    /// `tokio::time::timeout` re-polls its inner future at the deadline, so
    /// the SAME `write` future must be `Pending` on both its initial poll
    /// (poll 1) and its deadline re-poll (poll 2) for the `Elapsed` arm to
    /// fire; only the NEW `write` future created after that (poll 3+)
    /// resumes. It records every byte it accepts so the test can prove each
    /// byte is sent exactly once across the stall.
    struct ResumeAfterStallWriter {
        polls: Arc<AtomicUsize>,
        recorded: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for ResumeAfterStallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let p = self.polls.fetch_add(1, Ordering::SeqCst);
            if p == 1 || p == 2 {
                // Initial poll and deadline re-poll of the second `write`
                // future both stall, so `timeout` takes its `Elapsed` arm.
                return Poll::Pending;
            }
            let n = if p == 0 { buf.len().min(4) } else { buf.len() };
            self.recorded.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// A send stall (partial frame accepted, then no progress within
    /// `connTMO`) must emit `CircuitUnresponsive`, set the shared
    /// unresponsive flag, and KEEP the socket — never `TcpClosed`, and
    /// the loop must stay alive resuming the batch.
    #[tokio::test(start_paused = true)]
    async fn r2_40_send_stall_marks_unresponsive_and_keeps_socket() {
        let server_addr = test_addr();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending_frames = Arc::new(AtomicUsize::new(0));
        let unresponsive = Arc::new(UnresponsiveGate::new());
        let writer = PartialThenStallWriter {
            first_write: Arc::new(AtomicUsize::new(0)),
        };

        let task = tokio::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            0,
            event_tx,
            pending_frames.clone(),
            unresponsive.clone(),
        ));

        pending_frames.fetch_add(1, Ordering::SeqCst);
        write_tx
            .send(vec![0xAAu8; 32])
            .expect("frame enqueue must succeed");

        // First event on the stall must be CircuitUnresponsive (keep the
        // socket), never TcpClosed. `connection_timeout()` (default CONN_TMO 30 s)
        // elapses on the paused clock and the send watchdog fires.
        let evt = tokio::time::timeout(Duration::from_secs(60), event_rx.recv())
            .await
            .expect("write_loop must emit an event before 60 s")
            .expect("event channel must not be closed");
        match evt {
            TransportEvent::CircuitUnresponsive { server_addr: a, .. } => {
                assert_eq!(a, server_addr)
            }
            TransportEvent::TcpClosed { .. } => panic!(
                "R2-40: a send stall must mark the circuit unresponsive and \
                 KEEP the socket (C sendTimeoutNotify), not tear it down"
            ),
            _ => panic!("a send stall must emit CircuitUnresponsive"),
        }
        assert!(
            unresponsive.is_unresponsive(),
            "the shared unresponsive gate must be set on a send stall"
        );

        // No follow-up TcpClosed: the loop keeps the socket and retries
        // the same batch. The task must still be running.
        let none = tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await;
        assert!(
            none.is_err(),
            "a send stall must not follow up with TcpClosed; the socket is kept"
        );
        assert!(
            !task.is_finished(),
            "write_loop must keep running (resuming the batch), not exit on a stall"
        );
        task.abort();
    }

    /// After a mid-batch stall, the resume must send each byte exactly
    /// once from the tracked offset — a re-send from offset 0 would
    /// duplicate the accepted prefix and desync the server parser.
    #[tokio::test(start_paused = true)]
    async fn r2_40_resume_sends_each_byte_once_across_stall() {
        let server_addr = test_addr();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending_frames = Arc::new(AtomicUsize::new(1));
        let unresponsive = Arc::new(UnresponsiveGate::new());
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = ResumeAfterStallWriter {
            polls: Arc::new(AtomicUsize::new(0)),
            recorded: recorded.clone(),
        };

        let task = tokio::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            0,
            event_tx,
            pending_frames.clone(),
            unresponsive.clone(),
        ));

        let frame = vec![0xAAu8; 32];
        write_tx
            .send(frame.clone())
            .expect("frame enqueue must succeed");

        // The stall between the partial accept and the resume emits
        // exactly one CircuitUnresponsive.
        let evt = tokio::time::timeout(Duration::from_secs(60), event_rx.recv())
            .await
            .expect("write_loop must emit an event before 60 s")
            .expect("event channel must not be closed");
        assert!(
            matches!(evt, TransportEvent::CircuitUnresponsive { .. }),
            "the mid-batch stall must mark the circuit unresponsive"
        );

        // Let the loop resume and finish the batch on the paused clock.
        tokio::time::sleep(Duration::from_millis(1)).await;

        let got = recorded.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            32,
            "resume must send each byte exactly once: expected 32 bytes, got \
             {} (a re-send from offset 0 would double the 4-byte prefix)",
            got.len()
        );
        assert_eq!(
            got, frame,
            "resumed bytes must equal the original frame in order"
        );
        assert_eq!(
            pending_frames.load(Ordering::SeqCst),
            0,
            "the backpressure counter must be decremented once the batch flushed"
        );
        task.abort();
    }

    /// C `tcpSendWatchdog::expire` restarts (does NOT `sendTimeoutNotify`)
    /// while `receiveThreadIsBusy` (`tcpSendWatchdog.cpp:48-50`). Boundary
    /// test on that flag: a send stall while the recv thread is busy must
    /// NOT mark the circuit unresponsive; once the recv thread goes idle
    /// the same stall DOES. Fails in-flight IO with a spurious
    /// ECA_UNRESPTMO on a demonstrably-alive circuit if the guard is
    /// missing.
    #[tokio::test(start_paused = true)]
    async fn r2_40_send_stall_defers_while_recv_busy() {
        let server_addr = test_addr();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending_frames = Arc::new(AtomicUsize::new(1));
        let unresponsive = Arc::new(UnresponsiveGate::new());
        // Recv thread mid-processing when the send stalls.
        unresponsive.set_recv_busy(true);
        let writer = PartialThenStallWriter {
            first_write: Arc::new(AtomicUsize::new(0)),
        };

        let task = tokio::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            0,
            event_tx,
            pending_frames.clone(),
            unresponsive.clone(),
        ));

        write_tx
            .send(vec![0xAAu8; 32])
            .expect("frame enqueue must succeed");

        // While recv is busy the stall must restart, not mark unresponsive:
        // no event, gate stays responsive, across more than one stall cycle.
        let none = tokio::time::timeout(Duration::from_secs(70), event_rx.recv()).await;
        assert!(
            none.is_err(),
            "a send stall while the recv thread is busy must NOT emit an event \
             (C restarts the send watchdog)"
        );
        assert!(
            !unresponsive.is_unresponsive(),
            "recv-busy send stall must leave the circuit responsive"
        );

        // Recv thread goes idle — the next stall now marks unresponsive.
        unresponsive.set_recv_busy(false);
        let evt = tokio::time::timeout(Duration::from_secs(70), event_rx.recv())
            .await
            .expect("once recv is idle the send stall must emit within one cycle")
            .expect("event channel must not be closed");
        assert!(
            matches!(evt, TransportEvent::CircuitUnresponsive { .. }),
            "an idle-recv send stall marks the circuit unresponsive"
        );
        assert!(
            unresponsive.is_unresponsive(),
            "the gate must be set once the idle-recv stall fires"
        );
        task.abort();
    }
}

// Linux-only, not merely unix: the blackhole technique below (a listener
// with a full accept queue) relies on Linux answering an overflowing SYN
// with *silence* (`tcp_abort_on_overflow = 0`, the default), leaving the
// connecting peer in SYN-SENT. macOS/BSD instead answer the overflowing SYN
// with an RST, so `connect` resolves `Ok` immediately and the "no deadline"
// assertion becomes untestable there. The *production* invariant (no
// app-level deadline, matching `tcpiiu.cpp:606-661`) holds on every platform;
// only this way of exercising it is Linux-specific, so the test is scoped to
// where its blackhole actually blackholes.
#[cfg(all(test, target_os = "linux"))]
mod connect_deadline_tests {
    //! R7-19: the client must impose **no** application-level deadline on
    //! the TCP connect.
    //!
    //! C `tcpRecvThread::connect` (`tcpiiu.cpp:606-661`) issues a blocking
    //! `::connect()` and lets the OS TCP stack bound it — on Linux ~130 s
    //! of exponentially backed-off SYN retries. The port capped it at a
    //! hardcoded 5 s, so a server whose handshake is slow but alive (SYN
    //! loss, congested WAN) was reachable from a C client on the same wire
    //! and unreachable from this one.
    use super::*;
    use crate::client::types::{InFlightOps, ServerLastRxAt};
    use std::sync::Arc;
    use std::time::Duration;

    /// A local address whose SYNs the kernel drops: a listening socket
    /// with a full accept queue. Linux (`tcp_abort_on_overflow = 0`, the
    /// default) answers an overflowing SYN with silence, so the connecting
    /// peer sits in SYN-SENT and retries — exactly the "slow but live
    /// server" shape, without needing a route off the box.
    ///
    /// Returns the blackhole address and the listener, which the caller
    /// must keep alive (and must never accept from).
    fn syn_blackhole() -> (SocketAddr, socket2::Socket, std::net::TcpStream) {
        use socket2::{Domain, Socket, Type};

        let sock = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
        sock.bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .expect("bind");
        // Backlog 0 → the kernel rounds to a 1-slot accept queue.
        sock.listen(0).expect("listen");
        let addr: SocketAddr = sock.local_addr().expect("local_addr").as_socket().unwrap();

        // Fill the single accept-queue slot. This handshake completes; the
        // *next* SYN is the one that gets dropped. Held for the test's
        // lifetime so the queue stays full.
        let filler = std::net::TcpStream::connect(addr).expect("fill accept queue");

        (addr, sock, filler)
    }

    /// With `start_paused`, tokio auto-advances virtual time whenever the
    /// runtime is otherwise idle — so any timer left in the connect path
    /// fires immediately while the real SYN is still in flight. The outer
    /// 10-minute timeout is therefore the *only* timer that may fire: if
    /// it does, the connect had no deadline of its own, which is the
    /// contract. Pre-fix, the inner 5 s cap fired first and `connect_server`
    /// resolved to `None`.
    #[tokio::test(start_paused = true)]
    async fn tcp_connect_has_no_application_level_deadline() {
        let (addr, _listener, _filler) = syn_blackhole();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let identity = Arc::new(parking_lot::RwLock::new(
            crate::client::types::ClientIdentity::from_env(),
        ));
        let connect = connect_server(
            addr,
            0,
            event_tx,
            InFlightOps::new(),
            ServerLastRxAt::default(),
            identity,
            #[cfg(feature = "experimental-rust-tls")]
            None,
            #[cfg(feature = "experimental-rust-tls")]
            None,
        );

        let outcome = tokio::time::timeout(Duration::from_secs(600), connect).await;
        assert!(
            outcome.is_err(),
            "connect must still be in the OS's hands after 10 minutes of virtual time; \
             it resolved instead ({:?}), which means an application-level deadline fired \
             (C tcpiiu.cpp:606-661 has none)",
            outcome.map(|c| c.is_some())
        );
    }
}

/// The dial seam borrows its thread from a bounded pool rather than creating
/// one per attempt.
///
/// `exec_backend`-gated because that is where `dial_blocking` exists at all —
/// the hosted arm of `dial_ca` is `tokio::net::TcpStream::connect` and has no
/// thread to bound. Linux-only for the same reason as
/// `connect_deadline_tests` above: the blackhole relies on Linux answering an
/// overflowing SYN with silence.
#[cfg(all(test, exec_backend, target_os = "linux"))]
mod dial_pool_tests {
    use super::*;
    use epics_base_rs::runtime::blocking_io::MAX_DIAL_WORKERS;

    /// A local address whose SYNs the kernel drops, with saturation
    /// *verified* rather than assumed.
    ///
    /// `connect_deadline_tests::syn_blackhole` fills the one-slot accept
    /// queue with a single connection and trusts it. That races: a handshake
    /// completing through the SYN queue lands in the accept queue a beat
    /// after the filler's `connect()` returns, so a dial issued right behind
    /// it can still be admitted (observed on the PVA side). One admitted dial
    /// out of the twelve below would quietly weaken the count this test is
    /// about, so this copy keeps adding established fillers until a fresh
    /// nonblocking connect is still unanswered 300 ms in — on loopback a
    /// queued handshake completes in microseconds and a dropped SYN's first
    /// retransmit is at ~1 s, so an unanswered probe proves the queue is
    /// full. Closing the probe in SYN-SENT aborts the attempt, so it cannot
    /// steal a slot later.
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

    /// Every thread this client creates for a dial, whatever shape the dial
    /// seam has.
    ///
    /// Both prefixes on purpose: `"CAC-connect <addr>"` is the per-attempt
    /// thread the pool replaced and `"CAC-dial <n>"` is a pool worker, so a
    /// count taken through this function is comparable across the change and
    /// the bound below is asserted against the old shape as well as the new
    /// one. (`comm` is truncated to 15 bytes, so both are matched on their
    /// stable prefix.)
    fn dial_threads() -> usize {
        std::fs::read_dir("/proc/self/task")
            .expect("task dir")
            .filter(|e| {
                let Ok(e) = e else { return false };
                std::fs::read_to_string(e.path().join("comm"))
                    .is_ok_and(|c| c.starts_with("CAC-connect") || c.starts_with("CAC-dial"))
            })
            .count()
    }

    /// The bound the dial seam owes the target: **N dial attempts must not
    /// cost N threads.**
    ///
    /// Every `std::thread` leaks 128 B permanently on RTEMS — its TLS key is
    /// freed before the key's destructor runs — so the cost that matters is
    /// thread *creations*, not threads alive. `run_nameserver_connection`
    /// redials a failed address every `EPICS_CA_CONN_TMO` (30 s) for as long
    /// as the IOC runs, which is C's own cadence; under a per-attempt dial
    /// thread that is a leak with no ceiling.
    ///
    /// **The 200 ms bound below is the test's, not the client's.** R7-19
    /// (`connect_deadline_tests::tcp_connect_has_no_application_level_deadline`)
    /// is the standing rule that the *client* imposes no deadline on a
    /// connect, and this change does not touch it. The bound is here only to
    /// make the attempts sequential: without one a blackholed CA dial never
    /// resolves, so there would be no "next attempt" to count.
    ///
    /// The dials are aimed at a blackhole rather than at a refused port for a
    /// reason that is easy to get wrong. The production leak path — a name
    /// server that is *down* — refuses instantly, and a per-attempt thread
    /// serving it exits before this test could sample `/proc`, so counting
    /// live threads there would report ~0 and pass on the broken code. Under
    /// a blackhole every per-attempt thread is still pinned under the OS
    /// connect ladder and is countable, which makes the creation count
    /// observable by proxy.
    ///
    /// Fails on the per-attempt shape (12 live `CAC-connect` threads, one per
    /// attempt); passes on the pool (at most `MAX_DIAL_WORKERS`, and past the
    /// first four every further attempt queues).
    #[test]
    fn sequential_unanswered_dials_do_not_grow_the_dial_thread_count() {
        // Long enough to outgrow the bound it asserts, whatever the bound is
        // set to.
        const DIALS: usize = MAX_DIAL_WORKERS * 3;

        let (addr, _listener, _fillers) = syn_blackhole();
        let before = dial_threads();

        for i in 0..DIALS {
            let (tx, rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = tx.send(
                    epics_base_rs::runtime::task::timeout(
                        Duration::from_millis(200),
                        dial_blocking(addr),
                    )
                    .await
                    .is_err(),
                );
            });
            let timed_out = rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|e| panic!("attempt {i} must resolve at the test's bound: {e}"));
            assert!(
                timed_out,
                "attempt {i} toward a blackhole must still be in the OS's \
                 hands at the test's 200 ms bound"
            );
        }

        let after = dial_threads();
        assert!(
            after <= before + MAX_DIAL_WORKERS,
            "{DIALS} sequential dial attempts created {} dial threads (from \
             {before} to {after}); the dial seam must borrow from a bounded \
             set of at most {MAX_DIAL_WORKERS} permanent workers, not create \
             one per attempt — on RTEMS each creation leaks 128 B that is \
             never returned",
            after - before
        );
    }

    /// Boundary: an abandoned dial leaks no socket, and costs no worker.
    ///
    /// A caller that goes away mid-dial leaves the pool in one of exactly two
    /// states, and **both are correct** — which is why this test branches
    /// rather than assuming one of them:
    ///
    /// * the worker was already inside its `connect`, so a socket exists and
    ///   the worker must be its single finalizer — its send to the dropped
    ///   receiver fails and the fresh socket is dropped, and closed, before
    ///   it takes its next request;
    /// * the worker only reached the request *after* the caller had gone, saw
    ///   `Sender::is_closed()` and skipped the connect, so no socket was ever
    ///   opened.
    ///
    /// An earlier draft asserted the first unconditionally and hung under a
    /// loaded suite roughly half the time: between the single `poll` and the
    /// `drop` this test does a `/proc` scan, which is milliseconds of window
    /// for the worker to not yet have popped the request, and the blocking
    /// `accept` then waited forever for a connection that was correctly never
    /// made. Which branch is taken is genuinely a scheduling race, so it is
    /// the assertion that has to accommodate it — not a sleep.
    ///
    /// The worker itself deliberately does *not* retire either way; that is
    /// the bound. So the tail asserts the stronger property unconditionally:
    /// the same worker goes on to serve the next dial.
    #[test]
    fn abandoned_dial_leaks_no_socket_and_returns_its_worker() {
        use std::future::Future;

        let (addr, listener, fillers) = syn_blackhole();
        let mut fut = Box::pin(dial_blocking(addr));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        // The first poll submits the dial and parks on the oneshot.
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "a dial toward a blackhole must be pending, not resolved on the \
             caller's thread"
        );
        assert_eq!(
            dial_threads(),
            1,
            "the first poll must have handed the connect to a dial worker"
        );

        // The caller goes away mid-dial.
        drop(fut);

        // Un-blackhole: drain the fillers so that, if the worker did enter its
        // connect, the abandoned dial's SYN retry (~1 s, ~3 s) completes.
        for _ in 0..fillers.len() {
            let _ = listener.accept().expect("drain a filler");
        }
        drop(fillers);

        // `accept` honours `SO_RCVTIMEO` on Linux, so this is the branch
        // discriminator: a connection means the worker was mid-connect, and
        // `WouldBlock` past the SYN ladder's first two retries means it
        // skipped. Either way nothing may be left open.
        listener
            .set_read_timeout(Some(Duration::from_secs(6)))
            .expect("accept timeout");
        match listener.accept() {
            Ok((ours, _)) => {
                let ours: std::net::TcpStream = ours.into();
                ours.set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                use std::io::Read;
                let mut buf = [0u8; 1];
                let n = (&ours).read(&mut buf).expect("read on the abandoned dial");
                assert_eq!(
                    n, 0,
                    "the worker was inside its connect, so it owns the socket \
                     it opened and must close it once its receiver is gone"
                );
            }
            Err(e) if epics_base_rs::runtime::blocking_io::is_socket_timeout(e.kind()) => {
                // The worker reached the request after the caller had gone and
                // skipped the connect: no socket was opened, so there is none
                // to finalise.
            }
            Err(e) => panic!("accept on the abandoned dial: {e}"),
        }

        // …and it is back in service rather than retired: the next dial is
        // served without a second thread being created.
        let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let live_addr = live.local_addr().expect("addr");
        let acceptor = std::thread::spawn(move || live.accept().expect("accept").0);
        let (tx, rx) = std::sync::mpsc::channel();
        epics_base_rs::runtime::task::spawn(async move {
            let _ = tx.send(dial_blocking(live_addr).await.is_some());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(6))
                .expect("the next dial must resolve"),
            "a dial to a live listener must succeed"
        );
        assert_eq!(
            dial_threads(),
            1,
            "the worker that finalised the abandoned socket must serve the \
             next dial, not be replaced by a fresh thread"
        );
        drop(acceptor.join().expect("acceptor"));
    }
}

#[cfg(test)]
mod priority_circuit_tests {
    //! priority is part of the virtual-circuit identity. Two
    //! channels to the same IOC at different priorities open independent
    //! TCP circuits (libca `caServerID = (addr, priority)`), the VERSION
    //! message carries the priority in its `m_dataType` field, and
    //! tearing one priority circuit down leaves the other connected.
    use super::*;
    #[cfg(not(feature = "rtems-exec-model"))]
    use crate::client::types::{InFlightOps, ServerLastRxAt};
    use crate::protocol::{CA_PROTO_VERSION, CaHeader};
    #[cfg(not(feature = "rtems-exec-model"))]
    use std::collections::HashMap;
    use std::sync::Arc;
    #[cfg(not(feature = "rtems-exec-model"))]
    use std::time::Duration;
    #[cfg(not(feature = "rtems-exec-model"))]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(not(feature = "rtems-exec-model"))]
    use tokio::net::{TcpListener, TcpStream};

    /// Identity slot resolved from this process's env — the same source
    /// the production `CaClient` uses, so handshake byte lengths in these
    /// tests match what the manager emits.
    fn test_identity() -> crate::client::types::ClientIdentitySlot {
        Arc::new(parking_lot::RwLock::new(
            crate::client::types::ClientIdentity::from_env(),
        ))
    }

    /// Test 2 (deterministic, no sockets): the VERSION frame the client
    /// emits puts the requested priority in `m_dataType` and the minor
    /// version in `m_count` — exactly libca's `versionMessage` layout.
    #[test]
    fn version_message_carries_priority_in_data_type() {
        let identity = test_identity();
        for pri in [0u8, 1, 7, 99] {
            let hs = build_client_handshake(pri, &identity);
            let hdr = CaHeader::from_bytes(&hs[..16]).expect("parse VERSION header");
            assert_eq!(
                hdr.cmmd, CA_PROTO_VERSION,
                "first frame is CA_PROTO_VERSION"
            );
            assert_eq!(
                hdr.data_type, pri as u16,
                "VERSION m_dataType must equal the requested priority"
            );
            assert_eq!(
                hdr.count, CA_MINOR_VERSION,
                "VERSION m_count must still carry the minor protocol version"
            );
        }
    }

    /// R6-18: an identity frame is ALWAYS a plain 16-byte header.
    ///
    /// It is queued on connect, before the peer's VERSION frame can have
    /// arrived, so its peer version is unknown and the extended (24-byte)
    /// header — a CA_V49 feature — must never be emitted. C is in the
    /// same position and closes it with an assertion:
    /// `hostNameSetRequest` / `userNameSetRequest` both do
    /// `assert ( postSize < 0xffff )` (`tcpiiu.cpp:1303`, `:1333`) before
    /// handing the frame to `insertRequestHeader`. Pre-fix Rust emitted
    /// the annex for an over-long name, which a pre-V49 peer reads as a
    /// 65,535-byte body and de-syncs on.
    #[test]
    fn identity_frame_never_uses_the_extended_annex() {
        let small = build_identity_frame(crate::protocol::CA_PROTO_CLIENT_NAME, "operator");
        let (hdr, consumed) = CaHeader::from_bytes_extended(&small).expect("parse small frame");
        assert_eq!(hdr.cmmd, crate::protocol::CA_PROTO_CLIENT_NAME);
        assert_eq!(consumed, 16, "small payload stays in the 16-byte header");
        assert!(hdr.extended_postsize.is_none());
        assert_eq!(hdr.postsize as usize, small.len() - consumed);

        // A name past the 16-bit postsize cannot occur (C asserts; the IOC
        // caps a name at 512 bytes). If one is manufactured anyway, the
        // payload is clipped to the largest aligned size the plain header
        // can carry — never promoted to an annex.
        let big_value = "h".repeat(0x1_0000); // 65536 > 0xFFFF
        let big = build_identity_frame(crate::protocol::CA_PROTO_HOST_NAME, &big_value);
        let (hdr, consumed) = CaHeader::from_bytes_extended(&big).expect("parse big frame");
        assert_eq!(hdr.cmmd, crate::protocol::CA_PROTO_HOST_NAME);
        assert_eq!(consumed, 16, "identity frames are never extended");
        assert!(hdr.extended_postsize.is_none());
        assert_eq!(
            hdr.postsize, 0xFFF8,
            "clipped to the largest 8-aligned size the plain header carries"
        );
        assert_eq!(hdr.postsize as usize, big.len() - consumed);
        assert_eq!(big[big.len() - 1], 0, "payload stays NUL-terminated");
    }

    /// A rename written to the shared identity slot is reflected in the
    /// next circuit handshake: the CLIENT_NAME frame carries the new user
    /// name, not the env value the slot was seeded with.
    #[test]
    fn handshake_reflects_renamed_identity_slot() {
        let identity = test_identity();
        identity.write().user = "renamed-operator".to_string();
        let hs = build_client_handshake(0, &identity);

        let (vhdr, vconsumed) = CaHeader::from_bytes_extended(&hs).expect("parse VERSION");
        assert_eq!(vhdr.cmmd, CA_PROTO_VERSION);
        let (chdr, cconsumed) =
            CaHeader::from_bytes_extended(&hs[vconsumed..]).expect("parse CLIENT_NAME");
        assert_eq!(chdr.cmmd, crate::protocol::CA_PROTO_CLIENT_NAME);

        let payload_start = vconsumed + cconsumed;
        let payload = &hs[payload_start..payload_start + chdr.postsize as usize];
        let name = String::from_utf8_lossy(payload);
        assert_eq!(name.trim_end_matches('\0'), "renamed-operator");
    }

    /// Spawn a transport manager wired to fresh channels; return its
    /// command sender, event receiver, and the (observable) per-circuit
    /// writer registry.
    // Only the gated async circuit tests use this.
    #[cfg(not(feature = "rtems-exec-model"))]
    fn spawn_manager() -> (
        mpsc::UnboundedSender<TransportCommand>,
        mpsc::UnboundedReceiver<TransportEvent>,
        DirectServerWriters,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let in_flight = InFlightOps::new();
        let server_writers: DirectServerWriters = Arc::new(dashmap::DashMap::new());
        let last_rx_at: ServerLastRxAt = Arc::new(dashmap::DashMap::new());
        let observable = server_writers.clone();
        let identity = test_identity();
        #[cfg(not(feature = "experimental-rust-tls"))]
        tokio::spawn(run_transport_manager(
            cmd_rx,
            event_tx,
            in_flight,
            server_writers,
            last_rx_at,
            identity,
        ));
        #[cfg(feature = "experimental-rust-tls")]
        tokio::spawn(run_transport_manager(
            cmd_rx,
            event_tx,
            in_flight,
            server_writers,
            last_rx_at,
            identity,
            None,
            None,
            HashMap::new(),
        ));
        (cmd_tx, event_rx, observable)
    }

    /// Read the client's full handshake off a freshly accepted server
    /// socket and return the VERSION priority. Draining the whole
    /// handshake (its exact length is `build_client_handshake(pri).len()`
    /// because the test shares this process's USER/host env) leaves the
    /// socket positioned at the next frame, so a later read observes
    /// genuinely new traffic rather than buffered handshake bytes.
    // Only the gated async circuit tests use this.
    #[cfg(not(feature = "rtems-exec-model"))]
    async fn drain_handshake(stream: &mut TcpStream) -> u8 {
        let mut head = [0u8; 16];
        stream
            .read_exact(&mut head)
            .await
            .expect("read VERSION header");
        let hdr = CaHeader::from_bytes(&head).expect("parse VERSION");
        assert_eq!(hdr.cmmd, CA_PROTO_VERSION);
        let pri = hdr.data_type as u8;
        let total = build_client_handshake(pri, &test_identity()).len();
        let mut rest = vec![0u8; total - 16];
        stream
            .read_exact(&mut rest)
            .await
            .expect("drain handshake tail");
        pri
    }

    // Only the gated async circuit tests use this.
    #[cfg(not(feature = "rtems-exec-model"))]
    async fn wait_for_writers(sw: &DirectServerWriters, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while sw.len() < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected {n} circuit writers, saw {}",
                sw.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Test 1: two channels to the same server at different priorities
    /// open two independent transport circuit entries, and the server
    /// sees both priorities on the wire.
    // Spawns the async circuit manager, which has no tokio reactor
    // under `rtems-exec-model`; same reason as `server_connection_drop_tests`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_priorities_open_two_circuits() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let priorities = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let kept = Arc::new(tokio::sync::Mutex::new(Vec::<TcpStream>::new()));
        let pri_log = priorities.clone();
        let keep = kept.clone();
        let acceptor = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let pri = drain_handshake(&mut s).await;
                pri_log.lock().await.push(pri);
                keep.lock().await.push(s); // hold the socket so the circuit stays up
            }
        });

        let (cmd_tx, _event_rx, sw) = spawn_manager();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 1,
                pv_name: "X".into(),
                server_addr: addr,
                priority: 0,
            })
            .unwrap();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 2,
                pv_name: "Y".into(),
                server_addr: addr,
                priority: 7,
            })
            .unwrap();

        wait_for_writers(&sw, 2).await;
        assert!(
            sw.contains_key(&(addr, 0)),
            "priority-0 circuit writer missing"
        );
        assert!(
            sw.contains_key(&(addr, 7)),
            "priority-7 circuit writer missing"
        );
        assert_eq!(
            sw.len(),
            2,
            "two priorities to one server must yield two independent circuits"
        );

        let _ = tokio::time::timeout(Duration::from_secs(5), acceptor).await;
        let mut seen = priorities.lock().await.clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0u8, 7],
            "server observed both priorities on the wire"
        );
    }

    /// Test 3: dropping one priority circuit closes only that circuit;
    /// the sibling circuit at another priority stays connected and keeps
    /// carrying frames.
    // Spawns the async circuit manager, which has no tokio reactor
    // under `rtems-exec-model`; same reason as `server_connection_drop_tests`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_one_priority_circuit_leaves_the_other() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (sock_tx, mut sock_rx) = mpsc::unbounded_channel::<(u8, TcpStream)>();
        let acceptor = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let pri = drain_handshake(&mut s).await;
                let _ = sock_tx.send((pri, s));
            }
        });

        let (cmd_tx, mut event_rx, sw) = spawn_manager();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 1,
                pv_name: "X".into(),
                server_addr: addr,
                priority: 0,
            })
            .unwrap();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 2,
                pv_name: "Y".into(),
                server_addr: addr,
                priority: 5,
            })
            .unwrap();

        // Collect both server-side sockets, keyed by the priority each
        // negotiated.
        let mut socks: HashMap<u8, TcpStream> = HashMap::new();
        for _ in 0..2 {
            let (pri, s) = tokio::time::timeout(Duration::from_secs(5), sock_rx.recv())
                .await
                .expect("accept timed out")
                .expect("acceptor closed early");
            socks.insert(pri, s);
        }
        wait_for_writers(&sw, 2).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), acceptor).await;

        // Tear down the priority-0 circuit by closing its server socket.
        drop(socks.remove(&0).expect("priority-0 server socket"));

        // The client must report TcpClosed for priority 0 — and NEVER for
        // priority 5.
        let mut saw_zero_closed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !saw_zero_closed {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                })) => {
                    assert_eq!(server_addr, addr);
                    assert_ne!(
                        priority, 5,
                        "priority-5 circuit must not close when priority-0 is dropped"
                    );
                    if priority == 0 {
                        saw_zero_closed = true;
                    }
                }
                Ok(Some(_)) => {} // ServerConnected / ServerVersion / etc.
                Ok(None) => break,
                Err(_) => {} // 200ms tick, keep polling until the deadline
            }
        }
        assert!(
            saw_zero_closed,
            "dropping the priority-0 server socket must emit TcpClosed{{priority:0}}"
        );

        // The priority-5 circuit is still alive: its writer remains, and a
        // fresh frame sent on it actually reaches the server socket.
        assert!(
            sw.contains_key(&(addr, 5)),
            "priority-5 circuit writer must survive the priority-0 teardown"
        );
        cmd_tx
            .send(TransportCommand::ClearChannel {
                cid: 2,
                sid: 0,
                server_addr: addr,
                priority: 5,
            })
            .unwrap();
        let mut s5 = socks.remove(&5).expect("priority-5 server socket");
        let mut frame = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), s5.read_exact(&mut frame)).await;
        assert!(
            read.is_ok() && read.unwrap().is_ok(),
            "surviving priority-5 circuit must still carry frames after the sibling closed"
        );
        let _ = s5.shutdown().await;
    }
}

#[cfg(test)]
mod conn_tmo_env_tests {
    //! Per-boundary coverage of `EPICS_CA_CONN_TMO` resolution (R15-16).
    //!
    //! Every row was probed against the compiled C `caget`: the values C
    //! accepts must yield a working timeout here (no panic), and the ones
    //! `epicsScanDouble` rejects must fall back to the 30 s default.
    use super::resolve_connection_timeout;
    use std::time::Duration;

    /// SAFETY: gated by `serial_test::serial`; restored before return.
    fn with_env(value: Option<&str>, f: impl FnOnce()) {
        let saved = std::env::var("EPICS_CA_CONN_TMO").ok();
        unsafe {
            match value {
                Some(v) => std::env::set_var("EPICS_CA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_CA_CONN_TMO"),
            }
        }
        f();
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_CA_CONN_TMO"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn conn_tmo_boundaries() {
        let default = Duration::from_secs(30);
        let cases: &[(Option<&str>, Duration)] = &[
            // Unset and empty are both "use the compiled default" in C
            // (`envGetConfigParamPtr` folds "" back to unset).
            (None, default),
            (Some(""), default),
            // Valid, including sub-second and whitespace padding.
            (Some("10.5"), Duration::from_millis(10_500)),
            (Some(" 10.5 "), Duration::from_millis(10_500)),
            (Some("0.5"), Duration::from_millis(500)),
            // strtod takes C99 hex floats.
            (Some("0x10"), Duration::from_secs(16)),
            // `inf` is a value C accepts (errno stays clear): a deadline
            // that never fires. Used to panic the client.
            (Some("inf"), Duration::MAX),
            // ERANGE / no-conversion / extraneous → C's default branch.
            (Some("1e400"), default),
            (Some("abc"), default),
            (Some("10x"), default),
            (Some("   "), default),
            // NaN and non-positive values fail the `> 0.0` gate. C obeys them
            // and spins its connection watchdog; this is the documented
            // deviation at `resolve_connection_timeout`, and it is announced on
            // stderr rather than applied silently.
            (Some("nan"), default),
            (Some("0"), default),
            (Some("-5"), default),
        ];
        for (raw, want) in cases {
            with_env(*raw, || {
                assert_eq!(
                    resolve_connection_timeout(),
                    *want,
                    "EPICS_CA_CONN_TMO={raw:?} must resolve to {want:?}"
                );
            });
        }
    }
}

#[cfg(test)]
mod framing_tests {
    //! Per-boundary coverage of [`next_frame`], the CA client's one TCP
    //! framing step.
    //!
    //! These cases used to have no home. The rules lived twice — once in
    //! `read_loop`, once inline in `client/search.rs::run_nameserver_connection`
    //! — and each copy was only reachable through a live socket, so both were
    //! tested (when at all) end to end through whichever circuit owned them.
    //! That is exactly how the two copies drifted: the misaligned-postsize
    //! close and the partial-extended-header wait were each fixed twice,
    //! separately. One function, one boundary table, both callers.
    //!
    //! By boundary, not by scenario: the input is a byte prefix and the
    //! interesting values are the lengths at which the answer changes —
    //! 15/16 for the base header, 23/24 for the extended annex — plus the
    //! alignment predicate on either header form.
    use super::{Frame, FrameError, next_frame};
    use crate::protocol::{CA_PROTO_SEARCH, CaHeader};

    /// A 16-byte base header with the given raw `postsize`, no annex.
    fn base_header(postsize: u16) -> Vec<u8> {
        let mut hdr = CaHeader::new(CA_PROTO_SEARCH);
        hdr.postsize = postsize;
        hdr.to_bytes().to_vec()
    }

    /// A 24-byte extended header (`postsize == 0xFFFF`) declaring
    /// `ext_postsize` payload bytes.
    fn extended_header(ext_postsize: u32) -> Vec<u8> {
        let mut buf = base_header(0xFFFF);
        buf.extend_from_slice(&ext_postsize.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes()); // extended count
        buf
    }

    fn header_of(frame: Frame) -> (usize, usize) {
        match frame {
            Frame::Header {
                hdr_size, body_len, ..
            } => (hdr_size, body_len),
            Frame::Incomplete => panic!("expected a parsed header, got Incomplete"),
            Frame::Malformed(e) => panic!("expected a parsed header, got Malformed({e})"),
        }
    }

    /// Boundary: 0..=15 bytes cannot name a message. Every length below the
    /// base header is "read more", never a close — a TCP segment boundary in
    /// the middle of a header is ordinary.
    #[test]
    fn under_a_base_header_is_incomplete_at_every_length() {
        let full = base_header(0);
        for n in 0..CaHeader::SIZE {
            assert!(
                matches!(next_frame(&full[..n]), Frame::Incomplete),
                "{n} bytes is short of a 16-byte header and must be Incomplete"
            );
        }
    }

    /// The other side of that boundary: exactly 16 bytes with an empty body
    /// is a whole message, and the body it declares is zero — so a caller
    /// that adds `hdr_size + body_len` consumes 16 and no more.
    #[test]
    fn exactly_a_base_header_with_no_body_is_a_whole_message() {
        assert_eq!(header_of(next_frame(&base_header(0))), (16, 0));
    }

    /// Boundary: `postsize == 0xFFFF` promises an 8-byte annex, so 16..=23
    /// bytes are still "read more". This is the case a pure 16-byte parse
    /// gets catastrophically wrong — it would read the sentinel as a literal
    /// size and consume 65,540 bytes for a message whose true length is
    /// `24 + payload`.
    #[test]
    fn a_partial_extended_annex_is_incomplete_not_malformed() {
        let full = extended_header(8);
        for n in CaHeader::SIZE..24 {
            assert!(
                matches!(next_frame(&full[..n]), Frame::Incomplete),
                "{n} bytes is short of the 24-byte extended header and must be \
                 Incomplete, not a close"
            );
        }
        assert_eq!(header_of(next_frame(&full)), (24, 8));
    }

    /// The header is complete the moment its own bytes are present; the body
    /// arriving later is the caller's business, not the framer's.
    ///
    /// This is the contract that lets one function serve both circuits: the
    /// data circuit needs the length of a body it has not received yet (to
    /// decide whether to drain it past `EPICS_CA_MAX_ARRAY_BYTES`), the
    /// name-service circuit just waits. A framer that returned `Incomplete`
    /// here could not serve the first caller at all.
    #[test]
    fn a_header_parses_before_its_body_arrives() {
        let mut buf = base_header(64);
        buf.extend_from_slice(&[0u8; 8]); // 8 of the 64 body bytes
        assert_eq!(header_of(next_frame(&buf)), (16, 64));
    }

    /// C `tcpiiu.cpp::processIncoming:1198` closes the connection when
    /// `m_postsize & 0x7 != 0`. Every unaligned value below 8 must reach that
    /// close — rounding one of them up (the pre-fix `align8`) is what let a
    /// hostile peer slide the framer into the middle of the next message.
    #[test]
    fn every_misaligned_base_postsize_is_malformed() {
        for postsize in 1..8u16 {
            match next_frame(&base_header(postsize)) {
                Frame::Malformed(FrameError::MisalignedPayload(n)) => {
                    assert_eq!(n as u16, postsize)
                }
                Frame::Header { body_len, .. } => {
                    panic!("postsize={postsize} is misaligned but parsed as body_len={body_len}")
                }
                Frame::Incomplete => panic!("a whole 16-byte header is not Incomplete"),
                Frame::Malformed(e) => panic!("wrong refusal for postsize={postsize}: {e}"),
            }
        }
    }

    /// The alignment rule applies to the *actual* payload size, so it must
    /// still bite when that size came from the extended annex rather than
    /// from the base header's `m_postsize`.
    #[test]
    fn a_misaligned_extended_postsize_is_malformed() {
        assert!(matches!(
            next_frame(&extended_header(12)),
            Frame::Malformed(FrameError::MisalignedPayload(12))
        ));
        // The aligned neighbour on either side is accepted, so the assertion
        // above is about alignment and not about the annex.
        assert_eq!(header_of(next_frame(&extended_header(8))), (24, 8));
        assert_eq!(header_of(next_frame(&extended_header(16))), (24, 16));
    }

    /// Walking a buffer of chained messages is what both callers actually do,
    /// and the answer must be positional: the same bytes at a different
    /// offset yield the same framing. Mixed base and extended forms, because
    /// a stream that alternates is where a wrong `hdr_size` desyncs.
    #[test]
    fn chained_messages_are_consumed_one_at_a_time() {
        let mut stream = Vec::new();
        let mut expected = Vec::new();
        for (hdr, body) in [(base_header(8), 8usize), (extended_header(16), 16)] {
            expected.push(hdr.len() + body);
            stream.extend_from_slice(&hdr);
            stream.extend(std::iter::repeat_n(0u8, body));
        }
        stream.extend_from_slice(&base_header(0)[..7]); // a torn third header

        let mut offset = 0;
        let mut consumed = Vec::new();
        loop {
            match next_frame(&stream[offset..]) {
                Frame::Header {
                    hdr_size, body_len, ..
                } => {
                    let msg_len = hdr_size + body_len;
                    assert!(offset + msg_len <= stream.len(), "body must be present");
                    consumed.push(msg_len);
                    offset += msg_len;
                }
                Frame::Incomplete => break,
                Frame::Malformed(e) => panic!("well-formed chain refused: {e}"),
            }
        }
        assert_eq!(consumed, expected);
        assert_eq!(
            stream.len() - offset,
            7,
            "the torn header must be left in the buffer for the next read"
        );
    }
}

#[cfg(test)]
mod recv_body_policy_tests {
    //! Per-boundary coverage of [`RecvBodyPolicy`], the client's one
    //! receive-side body limit — the body-policy half of the seam whose
    //! framing half is `framing_tests` above, and shared by the same two
    //! callers (`read_loop`, the name-service reader). The limit is injected
    //! (`with_limit`) rather than read from `EPICS_CA_AUTO_ARRAY_BYTES` /
    //! `EPICS_CA_MAX_ARRAY_BYTES`, because these run under a parallel test
    //! runner and the environment is process-global.
    use super::RecvBodyPolicy;
    use std::net::SocketAddr;

    fn peer() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// `None` is C's default (`EPICS_CA_AUTO_ARRAY_BYTES=YES`): no body is
    /// ever refused, whatever the header announces.
    #[test]
    fn no_limit_refuses_nothing() {
        let mut policy = RecvBodyPolicy::with_limit(None);
        assert!(!policy.refuses(peer(), 0));
        assert!(!policy.refuses(peer(), usize::MAX));
    }

    /// Boundary: `body_len == limit` is admitted, `limit + 1` is refused —
    /// C's `if (msgSize > maxBytes)` is strict (`tcpiiu.cpp:1269`), and a
    /// refusal is sticky per message, not per circuit: the frame after a
    /// refused one is judged on its own size.
    #[test]
    fn refusal_boundary_is_strictly_over_the_limit() {
        let mut policy = RecvBodyPolicy::with_limit(Some(1024));
        assert!(!policy.refuses(peer(), 1023));
        assert!(!policy.refuses(peer(), 1024));
        assert!(policy.refuses(peer(), 1025));
        assert!(
            !policy.refuses(peer(), 1024),
            "an in-limit frame after a refused one must still be admitted"
        );
        assert!(
            policy.refuses(peer(), 1025),
            "refusal must not be one-shot — every over-limit frame is dropped"
        );
    }

    /// Drain accounting at every boundary of `owed` vs the bytes on hand:
    /// short reads keep draining, the exact read finishes clean, and an
    /// over-read leaves the surplus at the head of the buffer for framing.
    #[test]
    fn drain_refused_accounts_exactly_at_every_boundary() {
        // Nothing owed: a no-op that never signals "keep draining".
        let mut policy = RecvBodyPolicy::with_limit(Some(8));
        let mut acc = vec![1u8, 2, 3];
        assert!(!policy.drain_refused(&mut acc));
        assert_eq!(acc, [1, 2, 3]);

        // owed > present: everything is swallowed and more is owed.
        policy.owe(5);
        let mut acc = vec![0u8; 3];
        assert!(policy.drain_refused(&mut acc));
        assert!(acc.is_empty());

        // owed == present (the 2 remaining): swallowed, drain complete.
        let mut acc = vec![0u8; 2];
        assert!(!policy.drain_refused(&mut acc));
        assert!(acc.is_empty());

        // owed < present: only the owed prefix goes; the surplus is the next
        // message's bytes and must survive for the framer.
        policy.owe(2);
        let mut acc = vec![9u8, 9, 7, 7];
        assert!(!policy.drain_refused(&mut acc));
        assert_eq!(acc, [7, 7], "surplus bytes belong to the next frame");
    }
}

#[cfg(test)]
mod runtime_seam_guard {
    //! The CA circuit path must reach the runtime only through the seam.
    //!
    //! This module is the CA-client twin of `calink`'s
    //! `calink_production_spawns_go_through_the_runtime_seam` and of the two
    //! PVA/pvalink timer guards, and it exists because those guards had a
    //! hole this file fell straight through. Stage C3 pinned *spawns*; the
    //! pvalink stage-5 measurement then found that a task moved onto the
    //! callback pool takes its **timers** with it and grew a timer half. Two
    //! shapes were still unpinned here, and the target found both
    //! (`doc/calink-rtems-design.md` §11.1):
    //!
    //! * `tokio::task::JoinSet::spawn` — a fourth spelling of `tokio::spawn`,
    //!   which no "no bare `tokio::spawn`" needle matches;
    //! * `tokio::time::*` on `read_loop` / `write_loop`, the two pumps that
    //!   run on *every* circuit on the target.
    //!
    //! Both panic at runtime, on the target, with a green `cargo check` and a
    //! green host suite — the exact failure mode a source guard is for.

    /// Production slice: everything before the first column-0 *inline* `mod`
    /// (`mod name {`) — every one of which, in both files below, is a test
    /// module. A column-0 `mod name;` declaration is not a boundary:
    /// `client/mod.rs` opens with eight of them.
    ///
    /// NOT "the first `#[cfg(test)]`": `transport.rs` has a `#[cfg(test)]`
    /// *fn* at column 0 near the top (`drained_socket_probe`), and cutting
    /// there would shrink the slice past every line this guard exists to
    /// cover. Nor "the first `#[cfg(test)] mod`", which silently skips a
    /// module gated `#[cfg(all(test, …))]` and swallows it into the
    /// production slice — a slice rule that *grows* the covered region when a
    /// test module is gated is the wrong shape. Cutting at the `mod` line
    /// itself is invariant to which attribute precedes it. The MUST_CONTAIN
    /// anchors below fail closed if this ever slips again.
    fn production(src: &'static str) -> &'static str {
        let mut off = 0usize;
        for line in src.split_inclusive('\n') {
            if line.starts_with("mod ") && line.trim_end().ends_with('{') {
                return &src[..off];
            }
            off += line.len();
        }
        src
    }

    /// Every file the target actually runs on a callback band, each with the
    /// anchors its production slice must still contain. The anchors are two
    /// kinds at once and deliberately so: structural ones (`async fn
    /// read_loop`) make a slice rule that silently shrinks fail here instead
    /// of passing vacuously, and seam ones (`runtime::task::TaskSet`) assert
    /// the positive — that this path reaches the runtime *through* the seam,
    /// not merely that it avoids the banned spellings by having no timers at
    /// all.
    const TARGET_LIVE: [(&str, &str, &[&str]); 2] = [
        (
            // The transport manager plus both circuit pumps: one instance of
            // each per virtual circuit, all on `cbMedium` on the target.
            "client/transport.rs",
            include_str!("transport.rs"),
            &[
                "async fn run_transport_manager",
                "async fn read_loop",
                "async fn write_loop",
                concat!("runtime::task", "::TaskSet"),
                concat!("runtime::task", "::sleep_until("),
            ],
        ),
        (
            // Every channel operation's round-trip bound. Target finding 2
            // (`doc/calink-rtems-design.md` §11.2): four `cbMedium` panics
            // from `tokio::time::timeout` in the channel read path, which the
            // transport-only guard could not see.
            "client/mod.rs",
            include_str!("mod.rs"),
            &[
                "impl CaChannel {",
                concat!("runtime::task", "::timeout("),
                concat!("runtime::task", "::timeout_at("),
            ],
        ),
    ];

    /// Drop whole-line comments, so the prose *explaining* why a shape is
    /// banned cannot itself trip the check. Trailing comments are kept —
    /// dropping them would need to parse string literals, and a needle in a
    /// trailing comment is a false positive worth taking over a false
    /// negative in code.
    fn code_only(src: &str) -> String {
        src.lines()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("//") || t.starts_with("*/") || t.starts_with("* "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn circuit_path_reaches_the_runtime_only_through_the_seam() {
        for (label, src, must_contain) in TARGET_LIVE {
            let code = code_only(production(src));

            for anchor in must_contain {
                assert!(
                    code.contains(anchor),
                    "{label}: production slice no longer contains `{anchor}`; either \
                     the slice rule broke before this guard could check anything, or \
                     this path stopped reaching the runtime through the seam"
                );
            }

            // Negative: no shape that needs a tokio runtime survives on the
            // target-live path. Each of these compiles fine for
            // `armv7-rtems-eabihf` and panics the callback-band worker at
            // runtime.
            for needle in [
                concat!("tokio::task", "::JoinSet"),
                concat!("tokio::time", "::"),
                concat!("tokio", "::spawn("),
            ] {
                assert_eq!(
                    code.matches(needle).count(),
                    0,
                    "{label} must not name `{needle}`: on the RTEMS target this file \
                     runs on a callback band with no tokio runtime in the process, \
                     and the failure is a runtime panic on the target, not a build \
                     error"
                );
            }
        }
    }
}
