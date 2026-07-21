//! S1a/S1b/S1c — blocking, thread-per-client CA server driver.
//!
//! This is the RTEMS-oriented front-end for the Channel Access server. It
//! mirrors C `rsrv`'s I/O model — one blocking OS thread per accepted TCP
//! client (C `camsgtask`, `camsgtask.c:41`), reading with a blocking `recv`
//! (`camsgtask.c:71`) and writing every reply on that same thread — instead
//! of the async `tokio` reactor that [`crate::server::tcp::handle_client`]
//! uses. It exists so the CA server can run on RTEMS, where `tokio`'s I/O
//! reactor (`mio`) does not build, over plain `std::net` blocking BSD sockets
//! (which DO build for `armv7-rtems-eabihf`, and work on hosted Unix too — so
//! this whole driver is host-compiled and host-tested).
//!
//! S1b adds the UDP name-search responder ([`BlockingCaServer::serve_udp_search`]),
//! the analogue of C's `CAS-UDP` thread (`cast_server`, `cast_server.c:113`):
//! a blocking `std::net::UdpSocket` `recv_from` loop that drives the shared
//! [`crate::server::udp::parse_search_datagram`] decode on the thread (again
//! via [`block_on_sync`] / `park_on`) and `sendto`s the reply, so a SEARCH
//! reply is byte-identical to the async responder. The socket is bound with
//! raw `libc` ([`bind_udp_search`]) so its `SO_REUSEADDR`/`SO_REUSEPORT`
//! shared-port setup needs no `socket2`. S1b covers name-search replies only:
//! no beacons, no multicast / broadcast-secondary socket, no same-source
//! FIONREAD coalescing (each datagram yields its own reply).
//!
//! # What it reuses
//!
//! The wire logic is NOT reimplemented. This driver constructs the shared
//! [`ClientState`] and drives the shared [`dispatch_message`] — the exact
//! per-command handlers the async server runs — to completion on the client
//! thread via [`block_on_sync`]. With no async runtime entered on this
//! thread, `block_on_sync` selects `park_on`: it polls the handler future and
//! parks the thread between polls. The handlers on the read/lifecycle path
//! only ever suspend on runtime-agnostic `tokio::sync` primitives (the DB /
//! PV / ACF locks), which another thread completes; for a local record they
//! are uncontended and the future is ready on the first poll, so the thread
//! never actually parks.
//!
//! # The send lock
//!
//! C serializes every write to one client's socket with `client->lock`
//! (`SEND_LOCK`, `server.h:221`), because two threads write it — the
//! `camsgtask` command thread and the `CAS-event` monitor thread
//! (`dbEvent.c:1016`). We model that with an `Arc<Mutex<TcpStream>>` write
//! handle. Both writers now exist (S1c): the client thread draining command
//! replies AND the per-client event thread ([`run_event_task`]) writing
//! monitor updates lock this same mutex, so the two never interleave a frame.
//!
//! # Scope and what is deferred
//!
//! Covered: GET/read, the connection lifecycle (handshake, channel
//! create/clear), MONITORS (S1c part a), and — as of S1c part (b) — WRITE /
//! WRITE_NOTIFY. EVENT_ADD / EVENT_CANCEL are served by dedicated branches
//! (ahead of the allowlist) that share the async server's
//! [`register_subscription`] / `send_event` parity logic and hand each
//! subscription's live reader to a second per-client event thread
//! ([`run_event_task`], the C `dbEvent` `event_task` analogue) instead of
//! spawning a task per subscription. EVENTS_ON / EVENTS_OFF flow-control the
//! shared `EventUser` the event thread's readers consult, so gating matches the
//! async server.
//!
//! WRITE / WRITE_NOTIFY (S1c part b) run the shared [`serve_write_head`] — the
//! SAME SID/type/access gates, payload convert, DB write, trap-write bracket,
//! and sync/error replies the async server runs (one copy) — driven to the
//! synchronous head on THIS thread via `park_on`. C `camsgtask` never blocks on
//! the async put-callback (`dbNotify.c` callDone fires later on a background
//! thread), so when a record's process cycle goes async the head returns the
//! completion receiver and the dispatch thread hands it to the event thread
//! ([`EventTaskControl::WriteComplete`]); the event thread awaits it in its
//! `select` and writes the deferred WRITE_NOTIFY reply under the SAME send lock,
//! so the dispatch thread stays responsive and there is ONE owner of async
//! socket writes. A plain fire-and-forget WRITE is always synchronous (no reply).
//! [`command_drives_without_spawn`] stays a *fail-closed* allowlist for
//! everything not handled by a dedicated branch.
//!
//! Not yet mirrored for WRITE_NOTIFY on the blocking driver: the async server's
//! CHANNEL-level put-callback supersede (a second WRITE_NOTIFY on a channel with
//! one in flight cancels the previous and replies ECA_PUTCBINPROG to it, C
//! `write_notify_action:1660-1707`). The blocking driver keeps no per-channel
//! in-flight-put map, so a second WRITE_NOTIFY is instead refused at the RECORD
//! level (`dbNotify` `S_db_Blocked` → ECA_PUTCBINPROG on the NEW request) — the
//! new request never displaces the pending one. Same completion correctness; the
//! supersede-vs-refuse boundary differs and is a later increment.
//!
//! The gateway `-no_cache` read-hook GET (`tcp.rs:3111`) is the one genuinely
//! async branch a READ can reach; it is unreachable for a local record
//! (`try_get_read_snapshot_local` returns `Some`). A blocking driver serving
//! a hooked (gateway) PV would drive that upstream fetch under `park_on` with
//! no reactor and the client thread would block — so this driver targets
//! local-record IOCs, and hooked-PV routing is S1c background-executor work.
//!
//! Framing parity is also coarser than `handle_client`: the malformed-frame
//! refuse-but-keep-serving edges (ECA_TOLARGE / ECA_DEFUNCT drain,
//! misalignment) that `handle_client` implements at `tcp.rs:1946-2075` are
//! not yet shared; a genuinely malformed frame ends the circuit here rather
//! than draining just that message. Partial frames on TCP segment boundaries
//! ARE handled (the loop waits for the rest). A future increment factors the
//! shared sans-io frame parser both loops call.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::access_security::AccessSecurityConfig;
use epics_base_rs::server::database::PvDatabase;
use tokio::sync::mpsc;

use crate::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN,
    CA_PROTO_ECHO, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL, CA_PROTO_EVENTS_OFF,
    CA_PROTO_EVENTS_ON, CA_PROTO_HOST_NAME, CA_PROTO_READ, CA_PROTO_READ_NOTIFY,
    CA_PROTO_READ_SYNC, CA_PROTO_SEARCH, CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY,
    CaHeader, ECA_UNAVAILINSERV, ca_v49,
};
use crate::server::outbox::{self, OutboxDrain};
use crate::server::tcp::{
    CancelInfo, ChannelTarget, ClientState, EventTaskControl, MonitorDelivery,
    SubscriptionDelivery, SubscriptionOutcome, WriteHeadOutcome, cancel_subscription_reply,
    dispatch_message, is_peer_disconnect, register_subscription, run_event_task, send_ca_error,
    serve_write_head,
};
use crate::server::udp::{self, SearchReplyBatch};

/// The ACF handle type [`ClientState::new`] expects. `tokio::sync::RwLock` is
/// a runtime-agnostic lock (it needs no reactor — another thread's release
/// wakes a waiter), so it is sound to acquire under `park_on` and builds for
/// RTEMS. It is NOT `tokio`'s socket/timer/spawn machinery.
type SharedAcf = Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>;

/// A blocking, thread-per-client CA TCP server.
///
/// Owns the listening socket and the shared database/ACF handed to every
/// client thread. Analogue of C `req_server` / the `CAS-TCP` accept thread
/// (`caservertask.c:62`).
pub struct BlockingCaServer {
    listener: TcpListener,
    db: Arc<PvDatabase>,
    acf: SharedAcf,
    tcp_port: u16,
    shutdown: AtomicBool,
}

impl BlockingCaServer {
    /// Bind the accept socket. The server does not start accepting until
    /// [`serve`](Self::serve) is called.
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        db: Arc<PvDatabase>,
        acf: SharedAcf,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let tcp_port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            db,
            acf,
            tcp_port,
            shutdown: AtomicBool::new(false),
        })
    }

    /// The actual bound address (useful when binding to port 0).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Run the accept loop until [`shutdown`](Self::shutdown) is requested.
    /// Blocks the calling thread; run it on its own `std::thread`. Each
    /// accepted connection gets a dedicated client thread (C `camsgtask`
    /// spawn, `caservertask.c:109`); client threads are detached and exit on
    /// disconnect.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    // A `shutdown()` wakes the blocking accept by dialing our
                    // own socket; the throwaway connection is dropped here.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let peer = match stream.peer_addr() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let db = self.db.clone();
                    let acf = self.acf.clone();
                    let tcp_port = self.tcp_port;
                    let spawned = thread::Builder::new()
                        .name(format!("CAS-client-blocking {peer}"))
                        .spawn(move || {
                            if let Err(e) = handle_client_blocking(stream, peer, db, acf, tcp_port)
                            {
                                tracing::debug!(
                                    target: "epics_ca_rs::server::blocking",
                                    %peer, error = %e,
                                    "blocking CA client ended with error"
                                );
                            }
                        });
                    if let Err(e) = spawned {
                        tracing::warn!(
                            target: "epics_ca_rs::server::blocking",
                            %peer, error = %e,
                            "failed to spawn blocking CA client thread"
                        );
                    }
                }
                Err(e) => {
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    tracing::warn!(
                        target: "epics_ca_rs::server::blocking",
                        error = %e, "blocking CA accept failed"
                    );
                }
            }
        }
    }

    /// Ask the accept loop to stop and unblock it. Idempotent. Also stops the
    /// UDP name-search responder ([`serve_udp_search`](Self::serve_udp_search)),
    /// which polls the same flag between its short-capped `recv_from` calls.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake the blocking `accept()` by connecting to ourselves; the loop
        // then observes the flag and returns.
        if let Ok(addr) = self.listener.local_addr() {
            let _ = TcpStream::connect(addr);
        }
    }

    /// The advertised CA TCP port — the value SEARCH replies carry so a
    /// client knows where to open the circuit.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// Run the blocking UDP name-search responder on the calling thread until
    /// [`shutdown`](Self::shutdown) is requested. Blocks; run it on its own
    /// `std::thread` (a real IOC runs it alongside [`serve`](Self::serve)).
    /// Analogue of the C `CAS-UDP` thread (`cast_server`, `cast_server.c:113`).
    /// Bind `socket` with [`bind_udp_search`](Self::bind_udp_search).
    ///
    /// Scope: name-search reply only. No beacons, no multicast /
    /// broadcast-secondary socket. Same-source FIONREAD coalescing IS applied
    /// (C `cast_server.c:268-281`): a burst of same-peer search datagrams is
    /// batched into one reply datagram, and a held batch is flushed early when
    /// the next datagram comes from a different peer (`cast_server.c:205-214`).
    /// SEARCH replies are byte-identical to the async responder because both
    /// drive the shared `udp::parse_search_datagram` and shape via
    /// `SearchReplyBatch`.
    pub fn serve_udp_search(&self, socket: UdpSocket) -> CaResult<()> {
        handle_udp_search_blocking(socket, self.db.clone(), self.tcp_port, &self.shutdown)
    }
}

/// Bind a blocking UDP name-search responder socket. Raw `libc` throughout
/// (no `socket2`), so the socket-option path builds for RTEMS. For a
/// well-known port, enables SO_REUSEADDR + SO_REUSEPORT *before* bind so
/// several IOCs can share the CA search port — C
/// `epicsSocketEnableAddressUseForDatagramFanout` (`caservertask.c:628`).
/// An ephemeral (`0`) port is left bare: the flags would let the kernel
/// join this socket to an unrelated reuse group and load-balance SEARCHes
/// away from it, so the async responder (`udp::bind_responder_socket`) and
/// C's PORT_ANY sockets both leave them off there.
pub fn bind_udp_search(addr: SocketAddrV4) -> std::io::Result<UdpSocket> {
    bind_udp_search_socket(addr)
}

#[cfg(unix)]
fn set_reuse_opt(fd: std::os::fd::RawFd, opt: libc::c_int) -> std::io::Result<()> {
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn bind_udp_search_socket(addr: SocketAddrV4) -> std::io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;
    // SAFETY: `socket()` returns a fresh owned fd or -1.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(socket)
}

#[cfg(not(unix))]
fn bind_udp_search_socket(addr: SocketAddrV4) -> std::io::Result<UdpSocket> {
    // Non-Unix host: plain bind with no pre-bind reuse. RTEMS is Unix-family,
    // so the shared-port path above is the one that matters for the target.
    UdpSocket::bind(addr)
}

/// The `FIONREAD` ioctl request — bytes pending in the socket receive queue.
/// C `rsrv`'s batch-up gate: hold accumulated replies while this is `> 0`,
/// flush at `0` (`camsgtask.c:55`, `cast_server.c:272`).
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

/// Bytes pending in the socket receive queue via `FIONREAD` — C `rsrv`'s
/// batch-up gate. Callers hold accumulated replies while this is `> 0` and
/// flush at `0` (`camsgtask.c:52-67`, `cast_server.c:268-281`). On any
/// `ioctl` error this returns `Err`, and every caller treats that as
/// "flush now" — matching C's `status < 0` branch — so an absent or wrong
/// FIONREAD never coalesces (byte-correct, just unbatched) and never hangs.
#[cfg(unix)]
fn pending_bytes<F: std::os::fd::AsRawFd>(sock: &F) -> std::io::Result<usize> {
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
        return Err(std::io::Error::last_os_error());
    }
    Ok(n.max(0) as usize)
}

#[cfg(not(unix))]
fn pending_bytes<F>(_sock: &F) -> std::io::Result<usize> {
    // No FIONREAD off Unix (RTEMS and the host CI are both Unix-family). Report
    // "unavailable" so callers flush every iteration — never coalesce — which
    // is byte-correct, just unbatched.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "FIONREAD unavailable on this platform",
    ))
}

/// Send one already-shaped SEARCH-reply datagram to `dst` over the blocking
/// socket. A send failure is logged, never fatal — C `cast_server` keeps
/// serving after a send error (`caserverio.c:214-222`).
fn send_udp_reply(socket: &UdpSocket, dst: SocketAddr, payload: &[u8]) {
    if let Err(e) = socket.send_to(payload, dst) {
        tracing::warn!(
            target: "epics_ca_rs::server::blocking",
            %dst, error = %e, "blocking UDP SEARCH-reply send failed"
        );
    }
}

/// Flush the held coalescing batch (if any) to the peer it was accumulated
/// for, clearing both the batch and its owner address. Called at a recv-queue
/// drain, on idle, and at shutdown. `batch_addr` is `Some` exactly when the
/// batch may hold replies, so gating on it keeps "held replies ⟹ known owner"
/// true by construction.
fn flush_held_batch(
    socket: &UdpSocket,
    batch: &mut SearchReplyBatch,
    batch_addr: &mut Option<SocketAddr>,
) {
    if let Some(dst) = batch_addr.take() {
        if let Some(dg) = batch.take_reply() {
            send_udp_reply(socket, dst, &dg);
        }
    }
}

/// Serve CA UDP name searches on `socket` until `shutdown` is set. C
/// `cast_server` (`cast_server.c:113`): `recvfrom`, decode VERSION + SEARCH,
/// `sendto` the reply. The decode/respond core is the shared
/// [`udp::parse_search_datagram`]; only the socket I/O is blocking-specific,
/// so replies are byte-identical to the async responder.
///
/// FIONREAD batch-up (C `cast_server.c:268-281`): the `SearchReplyBatch` is
/// held across datagrams and flushed only when the recv queue drains
/// (FIONREAD == 0 or errors); a same-source search burst thus coalesces into
/// one reply datagram. A held batch is flushed early when the next datagram
/// arrives from a different peer (`cast_server.c:205-214`), and on idle /
/// shutdown so a buffered reply is never stranded.
fn handle_udp_search_blocking(
    socket: UdpSocket,
    db: Arc<PvDatabase>,
    tcp_port: u16,
    shutdown: &AtomicBool,
) -> CaResult<()> {
    // Cap each blocking `recv_from` so the loop can observe `shutdown` between
    // datagrams. C `cast_server` blocks in `recvfrom` forever; this timeout is
    // only a clean-stop seam and is otherwise transparent to the protocol.
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(CaError::Io)?;
    // 64 KB = IPv4 max datagram, matching the async responder's recv buffer.
    let mut buf = vec![0u8; 64 * 1024];

    // The coalescing batch persists across datagrams so a same-source search
    // burst yields ONE reply datagram — C `cast_server.c:268-281` drains the
    // recv queue (FIONREAD) into a single `cas_send_dg_msg`. `batch_addr` is
    // the peer the held batch belongs to (C `client->addr`); it is `Some`
    // exactly when `batch` may hold replies.
    let mut batch = SearchReplyBatch::default();
    let mut batch_addr: Option<SocketAddr> = None;

    while !shutdown.load(Ordering::Acquire) {
        let (len, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // Idle cap fired: the recv queue is drained, so flush any held
            // batch (C flushes on FIONREAD == 0) and re-check `shutdown`.
            Err(ref e) if is_read_timeout(e.kind()) => {
                flush_held_batch(&socket, &mut batch, &mut batch_addr);
                continue;
            }
            // C `cast_server.c:171-179`: a UDP recv error never exits the
            // responder — an earlier reply drawing an ICMP port-unreachable
            // surfaces here as ECONNREFUSED/ECONNRESET. Log-and-continue.
            Err(ref e) if is_peer_disconnect(e.kind()) => continue,
            Err(e) => return Err(CaError::Io(e)),
        };
        if len < CaHeader::SIZE {
            continue;
        }

        // Peer-change flush (C `cast_server.c:205-214`): a held batch belongs
        // to its source; if this datagram is from a different peer, flush the
        // held batch to the old peer before adopting the new one.
        if let Some(prev) = batch_addr {
            if prev != src {
                if let Some(dg) = batch.take_reply() {
                    send_udp_reply(&socket, prev, &dg);
                }
            }
        }
        batch_addr = Some(src);

        // Drive the shared UDP decode on this thread into the held batch. Its
        // only suspension point is the DB name lookup, Ready on the first poll
        // for a local record, so `block_on_sync` selects `park_on` and needs
        // no runtime. Over-threshold sub-batches come back in `ready` and go
        // out immediately to this datagram's source (C `cas_send_dg_msg`
        // flushes each full batch as it fills).
        let mut ready: Vec<Vec<u8>> = Vec::new();
        if block_on_sync(udp::parse_search_datagram(
            &buf[..len],
            &db,
            tcp_port,
            src,
            &mut batch,
            &mut ready,
        ))
        .is_err()
        {
            return Err(CaError::Protocol(
                "blocking UDP responder: decode future not blockable in this thread context".into(),
            ));
        }
        for dg in &ready {
            send_udp_reply(&socket, src, dg);
        }

        // FIONREAD batch-up gate (C `cast_server.c:268-281`): flush the held
        // batch only when the recv queue is drained (or FIONREAD errors);
        // otherwise hold so the next same-source datagram coalesces into it.
        match pending_bytes(&socket) {
            Ok(0) | Err(_) => flush_held_batch(&socket, &mut batch, &mut batch_addr),
            Ok(_) => { /* more datagrams pending: hold to coalesce */ }
        }
    }
    // Shutdown requested: flush any held batch so a buffered reply is not lost.
    flush_held_batch(&socket, &mut batch, &mut batch_addr);
    Ok(())
}

/// Read/lifecycle commands whose shared dispatch handler drives to completion
/// without spawning a task or awaiting genuine network I/O — i.e. the ones
/// `park_on` can run on a runtime-less thread. This is an ALLOWLIST: it fails
/// closed. EVENT_ADD / EVENT_CANCEL (S1c part a) and WRITE / WRITE_NOTIFY (S1c
/// part b) are absent because they have dedicated branches ahead of it, NOT
/// because they are refused; any genuinely unknown command still is.
fn command_drives_without_spawn(cmmd: u16) -> bool {
    matches!(
        cmmd,
        CA_PROTO_VERSION
            | CA_PROTO_ECHO
            | CA_PROTO_HOST_NAME
            | CA_PROTO_CLIENT_NAME
            | CA_PROTO_CREATE_CHAN
            | CA_PROTO_READ
            | CA_PROTO_READ_NOTIFY
            | CA_PROTO_EVENTS_ON
            | CA_PROTO_EVENTS_OFF
            | CA_PROTO_READ_SYNC
            | CA_PROTO_CLEAR_CHANNEL
            | CA_PROTO_SEARCH
    )
}

/// `WouldBlock` / `TimedOut` from a blocking read mean the read timeout fired
/// (`SO_RCVTIMEO`, set portably via `std`'s `set_read_timeout`; only when a cap
/// is configured). Treated as an idle close, matching `handle_client`'s
/// inactivity branch.
fn is_read_timeout(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Write one complete frame to the socket under the send lock, then flush.
fn write_frame_locked(send_lock: &Mutex<TcpStream>, frame: &[u8]) -> std::io::Result<()> {
    let mut sock = send_lock.lock().expect("CA send-lock poisoned");
    sock.write_all(frame)?;
    sock.flush()
}

/// Drain every queued frame into the socket in arrival order, then flush
/// once. Single owner of "server bytes reach the socket" for the command
/// path — the blocking analogue of `drain_and_flush` (`tcp.rs:1627`), held
/// under the `client->lock` send-lock (`server.h:221`).
fn drain_outbox_locked(
    send_lock: &Mutex<TcpStream>,
    drain: &mut OutboxDrain,
) -> std::io::Result<()> {
    let mut sock = send_lock.lock().expect("CA send-lock poisoned");
    while let Some(frame) = drain.try_next() {
        sock.write_all(&frame)?;
    }
    sock.flush()
}

/// The blocking driver's per-subscription cancel handle. This is a SEPARATE
/// map from the async server's `SubscriptionEntry` (which stores a
/// `tokio::task::JoinHandle` the blocking driver has no runtime to produce):
/// the live producer runs on the one event thread, so here we keep only what
/// EVENT_CANCEL and disconnect teardown need — the owning channel SID, the sub
/// id (echoed in the cancel ACK, and the key the event thread drops on cancel),
/// the framed type/count for the cancel ACK, and the target so the producer
/// subscriber can be removed from its record/PV.
struct BlockingSub {
    channel_sid: u32,
    sub_id: u32,
    data_type: u16,
    data_count: u32,
    target: ChannelTarget,
}

/// Serve one CA client over a blocking `TcpStream`. C `camsgtask`
/// (`camsgtask.c:41`).
fn handle_client_blocking(
    stream: TcpStream,
    peer: SocketAddr,
    db: Arc<PvDatabase>,
    acf: SharedAcf,
    tcp_port: u16,
) -> CaResult<()> {
    let _ = stream.set_nodelay(true);
    // `None` (no configured cap) leaves the socket in pure blocking mode — C
    // `rsrv`'s default (`camsgtask` blocks in `recv` with no idle cap).
    // `set_read_timeout` sets SO_RCVTIMEO on Unix (incl. RTEMS) and is portable
    // to Windows, unlike the former raw-libc helper.
    stream
        .set_read_timeout(crate::server::tcp::inactivity_timeout())
        .map_err(CaError::Io)?;

    // One socket, two roles: a blocking read handle owned by this thread, and
    // a write handle behind the SEND_LOCK. `try_clone` dups the fd so a
    // future monitor thread (S1c) can write while this thread blocks on read
    // — exactly the C camsgtask / CAS-event split.
    let mut reader = stream.try_clone().map_err(CaError::Io)?;
    let send_lock = Arc::new(Mutex::new(stream));

    let (outbox, mut drain) = outbox::channel();
    let mut state = ClientState::new(acf, tcp_port, db.clone());
    state.apply_connection_identity(peer, None, None);

    // C `create_tcp_client` (`caservertask.c:1525`) sends an unsolicited
    // CA_PROTO_VERSION as the first server frame; libca treats every received
    // frame as a liveness beat, so send it before the read loop.
    {
        let mut hdr = CaHeader::new(CA_PROTO_VERSION);
        hdr.count = CA_MINOR_VERSION;
        write_frame_locked(&send_lock, &hdr.to_bytes()).map_err(CaError::Io)?;
    }

    // The CAS-event monitor thread — the analogue of C `dbEvent.c` `event_task`
    // (`~876`): a SECOND thread per client that blocks on this client's monitor
    // queues and, when `db_post_events` posts an update, writes it to the socket
    // under the SAME `send_lock` this read/dispatch thread drains its command
    // replies under (C `client->lock` serializes `camsgtask` + `event_task`,
    // `server.h:221`). It has no async runtime either: `run_event_task` is an
    // async fn driven by `park_on`; its readers suspend on the runtime-agnostic
    // `EvQue` wake (a `Notify`) and are woken cross-thread by `db_post_events`.
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<EventTaskControl>();
    let event_send_lock = send_lock.clone();
    let event_thread = thread::Builder::new()
        .name(format!("CAS-event-blocking {peer}"))
        .spawn(move || {
            let mut write = |frame: &[u8]| write_frame_locked(&event_send_lock, frame);
            if block_on_sync(run_event_task(ev_rx, &mut write)).is_err() {
                tracing::error!(
                    target: "epics_ca_rs::server::blocking",
                    %peer,
                    "CAS-event thread future not blockable (unexpected on a runtime-less thread)"
                );
            }
        })
        .map_err(CaError::Io)?;

    // The blocking driver's own subscription registry (see [`BlockingSub`]).
    let mut blk_subs: HashMap<u32, BlockingSub> = HashMap::new();

    // Run the read/dispatch loop in a closure so the producer teardown + event-
    // thread join below runs on EVERY exit path (EOF, peer-disconnect, idle,
    // handler error) — C ends `camsgtask` AND its `event_task` together on a
    // client disconnect, with no leaked thread.
    let result: CaResult<()> = (|| {
        let mut accumulated: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 8192];

        loop {
            // C `camsgtask.c:52-67`: before blocking in `recv`, flush accumulated
            // replies only when the socket has no more bytes pending (FIONREAD ==
            // 0) or FIONREAD errors; otherwise hold them so replies to pipelined
            // commands batch into one write. `cas_send_bs_msg(client, TRUE)` is
            // this drain. The per-command drain that used to run at the loop
            // bottom is gone — a single request/response still flushes here on the
            // next iteration (the client is then idle, so FIONREAD == 0), while a
            // pipelined burst coalesces into one write.
            match pending_bytes(&reader) {
                Ok(0) | Err(_) => match drain_outbox_locked(&send_lock, &mut drain) {
                    Ok(()) => {}
                    Err(ref e) if is_peer_disconnect(e.kind()) => break,
                    Err(e) => return Err(CaError::Io(e)),
                },
                Ok(_) => { /* more bytes pending: hold to coalesce pipelined replies */ }
            }

            let n = match reader.read(&mut buf) {
                Ok(0) => break, // graceful EOF
                Ok(n) => n,
                // Peer vanished mid-session (RST / broken pipe / truncated) is a
                // clean disconnect, not a server error — same rule as
                // `handle_client` (`is_peer_disconnect`).
                Err(ref e) if is_peer_disconnect(e.kind()) => break,
                Err(ref e) if is_read_timeout(e.kind()) => {
                    tracing::warn!(
                        target: "epics_ca_rs::server::blocking",
                        %peer, "CA client idle (SO_RCVTIMEO), closing"
                    );
                    break;
                }
                Err(e) => return Err(CaError::Io(e)),
            };
            accumulated.extend_from_slice(&buf[..n]);

            let mut offset = 0;
            while offset + CaHeader::SIZE <= accumulated.len() {
                let tail = &accumulated[offset..];
                // Partial extended-form header (16..24 bytes of a 0xffff-postsize
                // frame): wait for the annex. Mirrors `handle_client` tcp.rs:2001.
                if ca_v49(state.client_minor_version())
                    && tail.len() < 24
                    && tail[2] == 0xFF
                    && tail[3] == 0xFF
                {
                    break;
                }
                let (hdr, hdr_size) =
                    match CaHeader::from_bytes_for_peer(tail, state.client_minor_version()) {
                        Ok(v) => v,
                        // A partial header at a segment boundary parses as Err;
                        // wait for more bytes. (A truly malformed frame also lands
                        // here and ends the circuit — see the module docs on the
                        // deferred refuse-and-continue parity.)
                        Err(_) => break,
                    };
                let actual_post = hdr.actual_postsize();
                let msg_len = hdr_size + actual_post;
                if offset + msg_len > accumulated.len() {
                    break; // frame body not fully arrived yet
                }
                let payload =
                    accumulated[offset + hdr_size..offset + hdr_size + actual_post].to_vec();

                if hdr.cmmd == CA_PROTO_EVENT_ADD {
                    // Register the subscription with the SHARED parity logic in
                    // `HandOff` mode: it validates caps/dedup/type/mask/count/ACF,
                    // delivers the initial value into `outbox`, and hands back the
                    // live reader WITHOUT spawning (this thread has no runtime). The
                    // per-channel cap and duplicate-sub-id check consult the blocking
                    // driver's own map (`blk_subs`), not the async `subscriptions`.
                    let sid = hdr.cid;
                    let sub_id = hdr.available;
                    let sub_id_in_use = blk_subs.contains_key(&sub_id);
                    let outcome = block_on_sync(register_subscription(
                        &hdr,
                        &payload,
                        &state,
                        &outbox,
                        SubscriptionDelivery::HandOff,
                        || blk_subs.values().filter(|s| s.channel_sid == sid).count(),
                        sub_id_in_use,
                    ));
                    match outcome {
                        Ok(Ok(SubscriptionOutcome::HandedOff(r))) => {
                            // Flush the initial snapshot (already queued in `outbox`)
                            // to the socket NOW, before the event thread can deliver
                            // any later `db_post_events` update — preserving the
                            // initial-before-update order (C funnels both through the
                            // one `event_task` queue; here two threads share the
                            // socket, so we serialize by writing the initial first).
                            match drain_outbox_locked(&send_lock, &mut drain) {
                                Ok(()) => {}
                                Err(ref e) if is_peer_disconnect(e.kind()) => break,
                                Err(e) => return Err(CaError::Io(e)),
                            }
                            blk_subs.insert(
                                r.sub_id,
                                BlockingSub {
                                    channel_sid: r.channel_sid,
                                    sub_id: r.sub_id,
                                    data_type: r.data_type,
                                    data_count: r.data_count,
                                    target: r.target.clone(),
                                },
                            );
                            // Hand the live reader to the event thread; it frames
                            // every later update with the same `send_event` builder
                            // the async producer uses (byte-identical). If the event
                            // thread is already gone, the client is ending — drop it.
                            let _ = ev_tx.send(EventTaskControl::Add(Box::new(MonitorDelivery {
                                reader: r.reader,
                                target: r.target,
                                sub_id: r.sub_id,
                                data_type: r.data_type,
                                data_count: r.data_count,
                                denied: r.denied,
                                long_string_mode: r.long_string_mode,
                                req_hdr: hdr,
                                client_minor: r.client_minor,
                                stats: r.stats,
                            })));
                        }
                        // A refusal frame is already queued; keep serving.
                        Ok(Ok(SubscriptionOutcome::Refused)) => {}
                        Ok(Ok(SubscriptionOutcome::Spawned(_))) => {
                            unreachable!("HandOff mode never spawns a task")
                        }
                        Ok(Err(e)) => {
                            let _ = drain_outbox_locked(&send_lock, &mut drain);
                            return Err(e);
                        }
                        Err(_not_blockable) => {
                            return Err(CaError::Protocol(
                                "blocking CA driver: EVENT_ADD register not blockable in this \
                             thread context"
                                    .into(),
                            ));
                        }
                    }
                } else if hdr.cmmd == CA_PROTO_EVENT_CANCEL {
                    // Shared cancel wire logic (bad-SID / bad-mon-id / ACK), reading
                    // the addressed subscription from the blocking driver's own map.
                    let sub_id = hdr.available;
                    let sub_info = blk_subs.get(&sub_id).map(|s| CancelInfo {
                        channel_sid: s.channel_sid,
                        data_type: s.data_type,
                        data_count: s.data_count,
                    });
                    match cancel_subscription_reply(&hdr, &state, &outbox, sub_info) {
                        Ok(true) => {
                            if let Some(sub) = blk_subs.remove(&sub_id) {
                                // Stop the producer: drop the reader on the event
                                // thread, then remove the subscriber from record/PV.
                                let _ = ev_tx.send(EventTaskControl::Cancel(sub.sub_id));
                                match &sub.target {
                                    ChannelTarget::SimplePv(pv) => {
                                        let _ = block_on_sync(pv.remove_subscriber(sub.sub_id));
                                    }
                                    ChannelTarget::RecordField { record, .. } => {
                                        record.write().remove_subscriber(sub.sub_id);
                                    }
                                }
                            }
                        }
                        // `cancel_subscription_reply` returns only `Ok(true)` or `Err`.
                        Ok(false) => {}
                        Err(e) => {
                            // The CA_PROTO_ERROR is queued; ship it, then end the
                            // circuit (C `event_cancel_reply` RSRV_ERROR).
                            let _ = drain_outbox_locked(&send_lock, &mut drain);
                            return Err(e);
                        }
                    }
                } else if hdr.cmmd == CA_PROTO_WRITE || hdr.cmmd == CA_PROTO_WRITE_NOTIFY {
                    // Drive the SYNCHRONOUS head of the write on this dispatch
                    // thread via `park_on` (the DB/PV locks are runtime-agnostic).
                    // `serve_write_head` is the SAME wire logic the async server
                    // runs (one copy): SID/type/access gates, payload convert, the
                    // DB write, the trap-write bracket, and every sync/error reply.
                    // C `camsgtask` must NOT block on the async put-callback
                    // completion (`dbNotify.c` callDone fires later on a background
                    // thread), so an async record returns `AsyncPending` and this
                    // thread returns at once, staying responsive to further
                    // commands.
                    match block_on_sync(serve_write_head(&hdr, &payload, &state, &db, &outbox)) {
                        // Sync WRITE_NOTIFY reply / fire-and-forget WRITE / any
                        // error frame is already queued in `outbox`; it flushes at
                        // the loop top under the send lock.
                        Ok(Ok(WriteHeadOutcome::Done)) => {}
                        Ok(Ok(WriteHeadOutcome::AsyncPending(pending))) => {
                            // The record's process cycle went async. Do NOT park
                            // this thread on `rx`: hand the completion to the event
                            // thread, which awaits the oneshot in its `select` and
                            // writes the deferred WRITE_NOTIFY reply under the SAME
                            // send lock (single owner of async socket writes). A
                            // plain WRITE never reaches here (fire-and-forget is
                            // always synchronous). If the event thread is already
                            // gone the client is ending — drop the completion.
                            let _ = ev_tx.send(EventTaskControl::WriteComplete(Box::new(pending)));
                        }
                        Ok(Err(e)) => {
                            // A protocol violation (bad SID/type/payload) queued its
                            // error frame; ship it, then end the circuit — same rule
                            // as the async server (RSRV_ERROR).
                            let _ = drain_outbox_locked(&send_lock, &mut drain);
                            return Err(e);
                        }
                        Err(_not_blockable) => {
                            return Err(CaError::Protocol(
                                "blocking CA driver: WRITE head not blockable in this \
                                 thread context"
                                    .into(),
                            ));
                        }
                    }
                } else if command_drives_without_spawn(hdr.cmmd) {
                    // Drive the shared handler to completion on this thread. No
                    // async runtime is entered, so `block_on_sync` uses `park_on`:
                    // the handler may only suspend on runtime-agnostic tokio::sync
                    // locks (DB / PV / ACF), which for a local record are Ready on
                    // the first poll. Spawning commands are gated out above; the
                    // gateway read-hook GET is unreachable for a local record.
                    match block_on_sync(dispatch_message(
                        &hdr, &payload, &mut state, &db, &outbox, peer, None,
                    )) {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            // Ship anything an earlier handler in this burst (or
                            // this handler's own error frame) queued, then end.
                            let _ = drain_outbox_locked(&send_lock, &mut drain);
                            return Err(e);
                        }
                        Err(_not_blockable) => {
                            // Only possible inside a current-thread tokio runtime;
                            // a blocking client thread never enters one.
                            return Err(CaError::Protocol(
                                "blocking CA driver: handler not blockable in this thread context"
                                    .into(),
                            ));
                        }
                    }
                } else {
                    // Not handled by any dedicated branch or the spawn-free
                    // allowlist. Fail closed with a clean CA error and keep serving
                    // — never a panic or a silent drop.
                    let _ = send_ca_error(
                        &outbox,
                        &hdr,
                        ECA_UNAVAILINSERV,
                        0xFFFF_FFFF,
                        "CAS: command not yet supported by blocking CA driver",
                        state.client_minor_version(),
                    );
                }
                offset += msg_len;
            }
            if offset > 0 {
                accumulated.drain(..offset);
            }
            // The reply drain lives at the loop top (FIONREAD-gated), not here —
            // holding replies until the socket drains is the C batch-up rule
            // (`camsgtask.c:52-67`). The only in-body drain is the handler-error
            // path above, which flushes queued frames before ending the circuit.
        }

        Ok(())
    })();

    // Producer teardown (runs on every exit path): drop each subscriber from
    // its record/PV so no dangling `EvQue` post target survives this client —
    // the blocking analogue of `dispatch_message`'s disconnect drain.
    for (_, sub) in blk_subs.drain() {
        match &sub.target {
            ChannelTarget::SimplePv(pv) => {
                let _ = block_on_sync(pv.remove_subscriber(sub.sub_id));
            }
            ChannelTarget::RecordField { record, .. } => {
                record.write().remove_subscriber(sub.sub_id);
            }
        }
    }
    // Dropping the control sender ends `run_event_task`; join so the second
    // thread never leaks (C `db_close_events` + `event_task` exit).
    drop(ev_tx);
    let _ = event_thread.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, RecordProcessResult};
    use epics_base_rs::types::{DbFieldType, EpicsValue};
    use std::net::Ipv4Addr;

    /// The RTEMS constraint (S1): the blocking driver must not touch tokio's
    /// async net/timer/spawn machinery — those don't build for RTEMS and
    /// cannot be driven by `park_on`. `tokio::sync` (locks) IS allowed and is
    /// referenced by the `SharedAcf` alias. This is a static guard: if a
    /// future edit reaches for an async socket/timer/spawn or suspends a
    /// future directly instead of driving it via `park_on`, the test fails at
    /// compile-time-embedded source inspection. (Comments in this file
    /// deliberately avoid the forbidden literals so they cannot self-match.)
    #[test]
    fn blocking_driver_has_no_async_runtime_symbols() {
        let src = include_str!("blocking.rs");
        // Tokens are assembled with `concat!` so the forbidden literals do
        // not appear contiguously in this file — otherwise this very test
        // body would match itself under `include_str!`.
        let forbidden = [
            concat!("tokio", "::net"),
            concat!("tokio", "::time"),
            concat!("tokio", "::", "spawn"),
            concat!("block", "_in_place"),
            concat!(".", "await"),
        ];
        for token in forbidden {
            assert!(
                !src.contains(token),
                "blocking CA driver must not reference `{token}`: S1 RTEMS constraint \
                 (no async net/timer/spawn; drive futures via park_on)"
            );
        }
    }

    fn seed_db(pvs: &[(&str, EpicsValue)]) -> Arc<PvDatabase> {
        let db = Arc::new(PvDatabase::new());
        for (name, value) in pvs {
            block_on_sync(db.add_pv(name, value.clone()))
                .expect("no runtime on test thread")
                .expect("add_pv");
        }
        db
    }

    fn new_acf() -> SharedAcf {
        Arc::new(tokio::sync::RwLock::new(None))
    }

    /// Read exactly one whole CA frame (header + declared payload) so the
    /// next read starts on a frame boundary.
    fn read_one_frame(sock: &mut TcpStream) -> Vec<u8> {
        let mut hdr = [0u8; CaHeader::SIZE];
        sock.read_exact(&mut hdr).expect("read frame header");
        // Peer minor is our own request minor; extended-form is not exercised
        // by these small scalar frames.
        let postsize = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        let mut frame = hdr.to_vec();
        if postsize > 0 {
            let mut body = vec![0u8; postsize];
            sock.read_exact(&mut body).expect("read frame body");
            frame.extend_from_slice(&body);
        }
        frame
    }

    /// Read frames until one with `cmmd` is seen; return it. Guards against a
    /// hang with a read timeout on the client socket.
    fn read_until_cmmd(sock: &mut TcpStream, cmmd: u16) -> Vec<u8> {
        for _ in 0..64 {
            let frame = read_one_frame(sock);
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return frame;
            }
        }
        panic!("did not receive cmmd={cmmd} within 64 frames");
    }

    fn version_frame() -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.to_bytes().to_vec()
    }

    fn client_name_frame(name: &str) -> Vec<u8> {
        let padded = crate::protocol::pad_string(name);
        let mut h = CaHeader::new(CA_PROTO_CLIENT_NAME);
        h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    }

    fn host_name_frame(host: &str) -> Vec<u8> {
        let padded = crate::protocol::pad_string(host);
        let mut h = CaHeader::new(CA_PROTO_HOST_NAME);
        h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    }

    fn create_chan_frame(cid: u32, pv: &str) -> Vec<u8> {
        let padded = crate::protocol::pad_string(pv);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    }

    fn read_notify_frame(sid: u32, ioid: u32, dbr_type: u16) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
        h.data_type = dbr_type;
        h.count = 1;
        h.cid = sid;
        h.available = ioid;
        h.to_bytes().to_vec()
    }

    /// Build the READ_NOTIFY reply the SHARED dispatch handler produces for
    /// this request sequence, entirely in-process. Since both the async
    /// server and this blocking driver run the same `dispatch_message`, this
    /// is the async server's exact reply bytes — the byte-parity reference.
    fn reference_read_notify_reply(
        db: &Arc<PvDatabase>,
        pv: &str,
        cid: u32,
        ioid: u32,
        dbr_type: u16,
    ) -> Vec<u8> {
        let acf = new_acf();
        let mut state = ClientState::new(acf, 5064, db.clone());
        let peer: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        state.apply_connection_identity(peer, None, None);
        let (outbox, mut drain) = outbox::channel();

        let run = |state: &mut ClientState, frame: Vec<u8>| {
            let (hdr, hdr_size) =
                CaHeader::from_bytes_for_peer(&frame, state.client_minor_version()).unwrap();
            let payload = frame[hdr_size..].to_vec();
            block_on_sync(dispatch_message(
                &hdr, &payload, state, db, &outbox, peer, None,
            ))
            .unwrap()
            .unwrap();
        };
        run(&mut state, version_frame());
        run(&mut state, create_chan_frame(cid, pv));
        run(&mut state, read_notify_frame(1, ioid, dbr_type));

        let mut reply = None;
        while let Some(f) = drain.try_next() {
            if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_READ_NOTIFY {
                reply = Some(f);
            }
        }
        reply.expect("reference produced a READ_NOTIFY reply")
    }

    /// (i) End-to-end over a real TCP socket: handshake, CREATE_CHAN,
    /// READ_NOTIFY of a local record returns byte-correct DBR, identical to
    /// the shared dispatch handler's (= async server's) reply.
    #[test]
    fn read_notify_over_real_socket_matches_shared_dispatch() {
        const DBR_DOUBLE: u16 = 6;
        let db = seed_db(&[("BLK:PV", EpicsValue::Double(42.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();

        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Server greets with an unsolicited VERSION.
        let greeting = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        assert_eq!(
            u16::from_be_bytes([greeting[0], greeting[1]]),
            CA_PROTO_VERSION
        );

        // Handshake.
        c.write_all(&version_frame()).unwrap();
        c.write_all(&client_name_frame("tester")).unwrap();
        c.write_all(&host_name_frame("testhost")).unwrap();

        // Create the channel; capture the server-assigned sid.
        c.write_all(&create_chan_frame(0x1234, "BLK:PV")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]); // m_available = sid
        assert_eq!(sid, 1, "first channel gets sid 1 (next_sid starts at 1)");

        // Read it.
        let ioid = 0x7777;
        c.write_all(&read_notify_frame(sid, ioid, DBR_DOUBLE))
            .unwrap();
        let reply = read_until_cmmd(&mut c, CA_PROTO_READ_NOTIFY);

        // Header parity.
        assert_eq!(
            u16::from_be_bytes([reply[4], reply[5]]),
            DBR_DOUBLE,
            "data_type"
        );
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1, "count");
        assert_eq!(
            u32::from_be_bytes([reply[12], reply[13], reply[14], reply[15]]),
            ioid,
            "m_available echoes the request ioid"
        );
        // Payload decodes to the record value.
        let post = u16::from_be_bytes([reply[2], reply[3]]) as usize;
        assert_eq!(post, 8, "DBR_DOUBLE scalar payload is 8 bytes");
        let val = f64::from_be_bytes([
            reply[16], reply[17], reply[18], reply[19], reply[20], reply[21], reply[22], reply[23],
        ]);
        assert_eq!(val, 42.0);

        // Full byte-for-byte parity against the shared dispatch reply.
        let reference = reference_read_notify_reply(&db, "BLK:PV", 0x1234, ioid, DBR_DOUBLE);
        assert_eq!(
            reply, reference,
            "blocking driver must ship the shared dispatch handler's exact bytes"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    /// (ii) A client that disconnects mid-session terminates its server
    /// thread cleanly (peer-disconnect is Ok, not an error).
    #[test]
    fn client_disconnect_terminates_client_thread_cleanly() {
        let db = seed_db(&[("BLK:D", EpicsValue::Long(1))]);
        let server = Arc::new(BlockingCaServer::bind("127.0.0.1:0", db, new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        {
            let mut c = TcpStream::connect(addr).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
            c.write_all(&version_frame()).unwrap();
            c.write_all(&create_chan_frame(1, "BLK:D")).unwrap();
            let _ = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
            // Drop `c` → RST/FIN. The client thread's blocking read returns
            // 0 (EOF) or a peer-disconnect error; either exits cleanly.
        }

        // The server keeps serving: a second client still gets a greeting.
        let mut c2 = TcpStream::connect(addr).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let greeting = read_until_cmmd(&mut c2, CA_PROTO_VERSION);
        assert_eq!(
            u16::from_be_bytes([greeting[0], greeting[1]]),
            CA_PROTO_VERSION
        );
        drop(c2);

        server.shutdown();
        accept.join().unwrap();
    }

    /// (iii) The read timeout actually fires: a socket with a short
    /// `set_read_timeout` and no incoming data returns WouldBlock/TimedOut,
    /// which the driver classifies as an idle close.
    #[test]
    fn recv_timeout_std_fires_on_idle() {
        let (a, _b) = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).unwrap();
            let (server, _) = listener.accept().unwrap();
            (server, client)
        };
        a.set_read_timeout(Some(Duration::from_millis(150)))
            .expect("set_read_timeout");
        let mut a = a;
        let mut buf = [0u8; 8];
        let err = a.read(&mut buf).expect_err("idle read must time out");
        assert!(
            is_read_timeout(err.kind()),
            "read timeout must surface as WouldBlock/TimedOut, got {:?}",
            err.kind()
        );
    }

    /// The dedicated-branch commands (EVENT_ADD / EVENT_CANCEL from S1c part a,
    /// WRITE / WRITE_NOTIFY from S1c part b) are deliberately ABSENT from the
    /// spawn-free allowlist — they are handled by their own branches ahead of it,
    /// not refused. The allowlist still fails closed for a genuinely unknown
    /// command.
    #[test]
    fn dedicated_and_allowlisted_commands_are_classified() {
        // Handled by dedicated branches, so not spawn-free-allowlisted.
        assert!(!command_drives_without_spawn(CA_PROTO_WRITE));
        assert!(!command_drives_without_spawn(CA_PROTO_WRITE_NOTIFY));
        assert!(!command_drives_without_spawn(CA_PROTO_EVENT_ADD));
        assert!(!command_drives_without_spawn(CA_PROTO_EVENT_CANCEL));
        // Driven inline through the shared dispatch handler.
        assert!(command_drives_without_spawn(CA_PROTO_READ_NOTIFY));
        assert!(command_drives_without_spawn(CA_PROTO_CREATE_CHAN));
        // An unknown command falls through to the fail-closed refusal.
        assert!(!command_drives_without_spawn(0xEEEE));
    }

    /// A UDP VERSION prelude with a valid sequence number
    /// (`m_dataType == sequenceNoIsValid`) — libca prepends this to a search
    /// datagram. Makes the responder echo the seq in the reply VERSION and
    /// keep the CA_V411 placeholder.
    fn udp_version_prelude(seq: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.data_type = 1; // sequenceNoIsValid (caProto.h:128)
        h.cid = seq;
        h.to_bytes().to_vec()
    }

    /// A UDP CA_PROTO_SEARCH request for `pv`. `count` carries the client CA
    /// minor version (must be >= 4); `available` is echoed into the reply.
    fn udp_search_frame(cid: u32, pv: &str) -> Vec<u8> {
        let padded = crate::protocol::pad_string(pv);
        let mut h = CaHeader::new(CA_PROTO_SEARCH);
        h.data_type = 5; // DO_REPLY flag; the UDP path ignores it, present for realism
        h.cid = cid;
        h.available = cid;
        h.set_payload_size(padded.len(), CA_MINOR_VERSION as u32, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    }

    /// The reply datagram(s) the SHARED UDP decode produces for `datagram`,
    /// driven entirely in-process. Since the async responder and this blocking
    /// responder both run `udp::parse_search_datagram` + `shape_trailing`, this
    /// is the async responder's exact reply bytes — the byte-parity reference.
    fn reference_udp_reply(
        db: &Arc<PvDatabase>,
        datagram: &[u8],
        tcp_port: u16,
        src: SocketAddr,
    ) -> Vec<Vec<u8>> {
        let mut batch = SearchReplyBatch::default();
        let mut ready: Vec<Vec<u8>> = Vec::new();
        block_on_sync(udp::parse_search_datagram(
            datagram, db, tcp_port, src, &mut batch, &mut ready,
        ))
        .expect("decode blockable on test thread");
        if let Some(dg) = batch.shape_trailing() {
            ready.push(dg);
        }
        ready
    }

    /// (S1b-i) End-to-end over a real UDP socket: a VERSION+SEARCH datagram
    /// for a seeded PV draws a SEARCH reply whose bytes equal the shared UDP
    /// decode's output (= the async responder's reply), with the sid sentinel
    /// and the advertised TCP port.
    #[test]
    fn udp_search_reply_over_real_socket_matches_shared_decode() {
        let db = seed_db(&[("BLK:UDP", EpicsValue::Double(1.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let tcp_port = server.tcp_port();

        // Ephemeral UDP responder socket — never the real 5064 (workspace rule).
        let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let resp_addr = resp.local_addr().unwrap();

        let srv = server.clone();
        let udp_thread = thread::spawn(move || srv.serve_udp_search(resp));

        let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut dg = udp_version_prelude(0xABCD);
        dg.extend_from_slice(&udp_search_frame(0x42, "BLK:UDP"));
        client.send_to(&dg, resp_addr).unwrap();

        let mut rbuf = vec![0u8; 64 * 1024];
        let (n, from) = client.recv_from(&mut rbuf).expect("SEARCH reply");
        assert_eq!(from, resp_addr, "reply comes from the responder socket");
        let reply = rbuf[..n].to_vec();

        // Byte-parity against the shared decode driven in-process.
        let expected = reference_udp_reply(&db, &dg, tcp_port, client.local_addr().unwrap());
        assert_eq!(expected.len(), 1, "one search → one reply datagram");
        assert_eq!(
            reply, expected[0],
            "blocking UDP responder must ship the shared decode's exact bytes"
        );

        // Structural: leading VERSION echo, then a SEARCH reply carrying the
        // ~0U sid sentinel and the advertised TCP port in data_type.
        assert_eq!(
            u16::from_be_bytes([reply[0], reply[1]]),
            CA_PROTO_VERSION,
            "reply leads with a VERSION echo (CA_V411 peer)"
        );
        let s = &reply[CaHeader::SIZE..];
        assert_eq!(
            u16::from_be_bytes([s[0], s[1]]),
            CA_PROTO_SEARCH,
            "second message is the SEARCH reply"
        );
        assert_eq!(
            u16::from_be_bytes([s[4], s[5]]),
            tcp_port,
            "data_type = advertised TCP port"
        );
        assert_eq!(
            u32::from_be_bytes([s[8], s[9], s[10], s[11]]),
            u32::MAX,
            "sid = ~0U sentinel (use UDP source address)"
        );

        server.shutdown();
        udp_thread.join().unwrap().unwrap();
    }

    /// (S1b-i) An unknown PV draws NO UDP reply. C `search_reply_udp` has no
    /// DO_REPLY branch on the UDP path (only `search_reply_tcp` emits
    /// NOT_FOUND), so the responder is silent and the client recv times out.
    #[test]
    fn nonexistent_pv_gets_no_udp_reply() {
        let db = seed_db(&[("BLK:EXISTS", EpicsValue::Long(1))]);
        let server = Arc::new(BlockingCaServer::bind("127.0.0.1:0", db, new_acf()).unwrap());
        let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let resp_addr = resp.local_addr().unwrap();
        let srv = server.clone();
        let udp_thread = thread::spawn(move || srv.serve_udp_search(resp));

        let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let mut dg = udp_version_prelude(1);
        dg.extend_from_slice(&udp_search_frame(9, "BLK:DOES_NOT_EXIST"));
        client.send_to(&dg, resp_addr).unwrap();

        let mut rbuf = [0u8; 1024];
        let got = client.recv_from(&mut rbuf);
        assert!(
            got.as_ref().is_err_and(|e| is_read_timeout(e.kind())),
            "an unknown PV must draw NO UDP reply, got {got:?}"
        );

        server.shutdown();
        udp_thread.join().unwrap().unwrap();
    }

    /// (FIONREAD batch-up) A same-source search burst coalesces into ONE reply
    /// datagram. Both search datagrams are queued in the responder socket's
    /// recv queue BEFORE the responder starts, so the FIONREAD gate holds the
    /// first reply (queue not yet drained) and flushes both together on drain
    /// — C `cast_server.c:268-281`. Bytes equal the shared decode driven
    /// through ONE batch, and no second datagram is emitted.
    #[test]
    fn udp_same_source_burst_coalesces_into_one_reply() {
        let db = seed_db(&[("BLK:BURST", EpicsValue::Double(1.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let tcp_port = server.tcp_port();

        let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let resp_addr = resp.local_addr().unwrap();

        let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client_addr = client.local_addr().unwrap();

        // Queue both datagrams (localhost `send_to` enqueues synchronously)
        // BEFORE the responder reads anything, so the burst is guaranteed to
        // be in the recv queue when FIONREAD is checked after datagram 1.
        let mut dg1 = udp_version_prelude(0x1111);
        dg1.extend_from_slice(&udp_search_frame(0x01, "BLK:BURST"));
        let mut dg2 = udp_version_prelude(0x2222);
        dg2.extend_from_slice(&udp_search_frame(0x02, "BLK:BURST"));
        client.send_to(&dg1, resp_addr).unwrap();
        client.send_to(&dg2, resp_addr).unwrap();

        let srv = server.clone();
        let udp_thread = thread::spawn(move || srv.serve_udp_search(resp));

        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut rbuf = vec![0u8; 64 * 1024];
        let (n, from) = client.recv_from(&mut rbuf).expect("coalesced SEARCH reply");
        assert_eq!(from, resp_addr, "reply comes from the responder socket");
        let reply = rbuf[..n].to_vec();

        // No SECOND reply datagram: the same-source burst coalesced into one.
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let second = client.recv_from(&mut rbuf);
        assert!(
            second.as_ref().is_err_and(|e| is_read_timeout(e.kind())),
            "same-source burst must coalesce into ONE reply datagram, got another: {second:?}"
        );

        // Byte-parity: drive BOTH datagrams through the shared decode into ONE
        // batch (exactly what the responder's held batch does), then shape.
        let expected = {
            let mut batch = SearchReplyBatch::default();
            let mut ready: Vec<Vec<u8>> = Vec::new();
            block_on_sync(udp::parse_search_datagram(
                &dg1,
                &db,
                tcp_port,
                client_addr,
                &mut batch,
                &mut ready,
            ))
            .unwrap();
            block_on_sync(udp::parse_search_datagram(
                &dg2,
                &db,
                tcp_port,
                client_addr,
                &mut batch,
                &mut ready,
            ))
            .unwrap();
            assert!(
                ready.is_empty(),
                "two small searches stay under the mid-parse flush threshold"
            );
            batch.shape_trailing().expect("merged reply bytes")
        };
        assert_eq!(
            reply, expected,
            "coalesced reply must equal the shared decode's merged bytes"
        );

        // Structural: leads with ONE VERSION echo and carries strictly more
        // than a single datagram's reply (proving datagram 2's reply merged in).
        assert_eq!(
            u16::from_be_bytes([reply[0], reply[1]]),
            CA_PROTO_VERSION,
            "coalesced reply leads with a single VERSION echo"
        );
        let single = reference_udp_reply(&db, &dg1, tcp_port, client_addr);
        assert_eq!(single.len(), 1);
        assert!(
            reply.len() > single[0].len(),
            "coalesced reply must carry more than one datagram's worth of SEARCH replies"
        );

        server.shutdown();
        udp_thread.join().unwrap().unwrap();
    }

    /// (FIONREAD batch-up) A datagram from a different source flushes the held
    /// batch to the PRIOR peer before adopting the new one — C
    /// `cast_server.c:205-214`. Peer A's datagram, then peer B's, are queued
    /// before the responder starts; the responder holds A's reply (B pending),
    /// then B's arrival (a different source) flushes A's reply to A, and B's
    /// own reply flushes on queue drain. Each peer receives exactly its own
    /// reply bytes at its own address.
    #[test]
    fn udp_source_change_flushes_held_batch_to_prior_peer() {
        let db = seed_db(&[("BLK:SRC", EpicsValue::Double(2.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let tcp_port = server.tcp_port();

        let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let resp_addr = resp.local_addr().unwrap();

        // Two distinct client sockets = two distinct source addresses.
        let client_a = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client_b = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let a_addr = client_a.local_addr().unwrap();
        let b_addr = client_b.local_addr().unwrap();

        // Queue A then B before the responder starts (arrival order is A, B
        // because localhost `send_to` enqueues synchronously in call order).
        let mut dg_a = udp_version_prelude(0xAAAA);
        dg_a.extend_from_slice(&udp_search_frame(0x0A, "BLK:SRC"));
        let mut dg_b = udp_version_prelude(0xBBBB);
        dg_b.extend_from_slice(&udp_search_frame(0x0B, "BLK:SRC"));
        client_a.send_to(&dg_a, resp_addr).unwrap();
        client_b.send_to(&dg_b, resp_addr).unwrap();

        let srv = server.clone();
        let udp_thread = thread::spawn(move || srv.serve_udp_search(resp));

        client_a
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client_b
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut abuf = vec![0u8; 64 * 1024];
        let (an, afrom) = client_a.recv_from(&mut abuf).expect("A's reply");
        assert_eq!(
            afrom, resp_addr,
            "A's reply comes from the responder socket"
        );
        let a_reply = abuf[..an].to_vec();

        let mut bbuf = vec![0u8; 64 * 1024];
        let (bn, bfrom) = client_b.recv_from(&mut bbuf).expect("B's reply");
        assert_eq!(
            bfrom, resp_addr,
            "B's reply comes from the responder socket"
        );
        let b_reply = bbuf[..bn].to_vec();

        // Each peer gets exactly its own datagram's reply bytes — no
        // cross-contamination and no coalescing across the source change.
        let expect_a = reference_udp_reply(&db, &dg_a, tcp_port, a_addr);
        let expect_b = reference_udp_reply(&db, &dg_b, tcp_port, b_addr);
        assert_eq!(expect_a.len(), 1);
        assert_eq!(expect_b.len(), 1);
        assert_eq!(a_reply, expect_a[0], "peer A receives A's reply bytes");
        assert_eq!(b_reply, expect_b[0], "peer B receives B's reply bytes");

        // Neither peer receives a second datagram.
        client_a
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(
            client_a
                .recv_from(&mut abuf)
                .is_err_and(|e| is_read_timeout(e.kind())),
            "peer A must receive exactly one reply"
        );

        server.shutdown();
        udp_thread.join().unwrap().unwrap();
    }

    /// (FIONREAD batch-up, TCP) Two READ_NOTIFYs pipelined in one write each
    /// draw a byte-correct reply. C `camsgtask.c:52-67` holds replies while the
    /// socket has bytes pending and flushes when it drains, so the two replies
    /// leave in one batched write.
    ///
    /// The batching itself is NOT observable at the client: TCP is a byte
    /// stream, so two replies delivered in one `write` are indistinguishable
    /// from two `write`s — asserting "one write" would require instrumenting
    /// the server's socket writes, which this test deliberately does not fake.
    /// It asserts the observable contract instead: both replies arrive and
    /// each equals the shared dispatch handler's exact bytes.
    #[test]
    fn tcp_pipelined_read_notifies_each_get_correct_reply() {
        const DBR_DOUBLE: u16 = 6;
        let db = seed_db(&[("BLK:PIPE", EpicsValue::Double(7.5))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);

        c.write_all(&version_frame()).unwrap();
        c.write_all(&client_name_frame("tester")).unwrap();
        c.write_all(&host_name_frame("testhost")).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:PIPE")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

        // Two READ_NOTIFYs, different ioids, sent in ONE write (pipelined).
        let ioid1 = 0x0000_AA01;
        let ioid2 = 0x0000_AA02;
        let mut pipelined = read_notify_frame(sid, ioid1, DBR_DOUBLE);
        pipelined.extend_from_slice(&read_notify_frame(sid, ioid2, DBR_DOUBLE));
        c.write_all(&pipelined).unwrap();

        // Collect both READ_NOTIFY replies (order preserved on one circuit).
        let mut replies: Vec<Vec<u8>> = Vec::new();
        for _ in 0..64 {
            let frame = read_one_frame(&mut c);
            if u16::from_be_bytes([frame[0], frame[1]]) == CA_PROTO_READ_NOTIFY {
                replies.push(frame);
                if replies.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(replies.len(), 2, "both pipelined READ_NOTIFYs must reply");

        // Each reply echoes its own ioid in m_available (bytes 12..16).
        let ioid_of = |r: &[u8]| u32::from_be_bytes([r[12], r[13], r[14], r[15]]);
        assert_eq!(ioid_of(&replies[0]), ioid1, "first reply echoes ioid1");
        assert_eq!(ioid_of(&replies[1]), ioid2, "second reply echoes ioid2");

        // Byte-for-byte parity against the shared dispatch handler.
        let ref1 = reference_read_notify_reply(&db, "BLK:PIPE", 0x1234, ioid1, DBR_DOUBLE);
        let ref2 = reference_read_notify_reply(&db, "BLK:PIPE", 0x1234, ioid2, DBR_DOUBLE);
        assert_eq!(
            replies[0], ref1,
            "first reply must match shared dispatch bytes"
        );
        assert_eq!(
            replies[1], ref2,
            "second reply must match shared dispatch bytes"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    // ---- S1c part (a): the monitor event-task (EVENT_ADD / EVENT_CANCEL) ----

    /// DBR_DOUBLE: a plain scalar double monitor. It carries no timestamp
    /// field, so the encoded frame is deterministic — the byte-parity
    /// references below need not reproduce a wall-clock stamp.
    const DBR_DOUBLE_MON: u16 = 6;

    /// A CA_PROTO_EVENT_ADD (monitor subscribe) frame for a scalar DBR_DOUBLE
    /// with mask DBE_VALUE|DBE_ALARM — the 16-byte extended monitor request the
    /// async server also parses (cf. `event_add_frame` in the tcp.rs tests).
    fn event_add_double_frame(sid: u32, sub_id: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = DBR_DOUBLE_MON;
        h.count = 1;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, 1, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&0f32.to_be_bytes()); // low
        f.extend_from_slice(&0f32.to_be_bytes()); // high
        f.extend_from_slice(&0f32.to_be_bytes()); // to
        f.extend_from_slice(&3u16.to_be_bytes()); // mask: DBE_VALUE|DBE_ALARM
        f.extend_from_slice(&0u16.to_be_bytes()); // pad
        f
    }

    fn events_off_frame() -> Vec<u8> {
        CaHeader::new(CA_PROTO_EVENTS_OFF).to_bytes().to_vec()
    }

    fn events_on_frame() -> Vec<u8> {
        CaHeader::new(CA_PROTO_EVENTS_ON).to_bytes().to_vec()
    }

    /// The DBR_DOUBLE scalar value in an EVENT_ADD monitor reply (payload
    /// begins right after the 16-byte header).
    fn monitor_reply_value(frame: &[u8]) -> f64 {
        f64::from_be_bytes([
            frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22], frame[23],
        ])
    }

    /// Drain the outbox for its first CA_PROTO_EVENT_ADD (monitor) frame.
    fn drain_one_event_add(drain: &mut OutboxDrain) -> Vec<u8> {
        while let Some(f) = drain.try_next() {
            if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_EVENT_ADD {
                return f;
            }
        }
        panic!("outbox held no CA_PROTO_EVENT_ADD frame");
    }

    /// Look up a seeded simple PV so a test can post an update to it (the
    /// `db_post_events` analogue: `pv.set` fans out to subscribers).
    fn simple_pv(
        db: &Arc<PvDatabase>,
        name: &str,
    ) -> Arc<epics_base_rs::server::pv::ProcessVariable> {
        match block_on_sync(db.find_entry(name))
            .expect("blockable on test thread")
            .expect("seeded PV present")
        {
            epics_base_rs::server::database::PvEntry::Simple(pv) => pv,
            _ => panic!("{name} is not a simple PV"),
        }
    }

    /// The initial + first-update monitor frames the SHARED path produces for a
    /// DBR_DOUBLE subscription: `register_subscription` (the initial snapshot)
    /// and `send_event` (the update), driven entirely in-process. Both the
    /// async server and this blocking driver run these exact functions, so
    /// these are the async server's monitor bytes — the byte-parity reference.
    /// Uses its OWN db so the update posted here does not perturb the server
    /// under test.
    fn reference_monitor_frames(seed: f64, update: f64, sub_id: u32) -> (Vec<u8>, Vec<u8>) {
        let db = seed_db(&[("REF:MON", EpicsValue::Double(seed))]);
        let acf = new_acf();
        let mut state = ClientState::new(acf, 5064, db.clone());
        let peer: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        state.apply_connection_identity(peer, None, None);
        let (outbox, mut drain) = outbox::channel();

        let run = |state: &mut ClientState, frame: Vec<u8>| {
            let (hdr, hdr_size) =
                CaHeader::from_bytes_for_peer(&frame, state.client_minor_version()).unwrap();
            let payload = frame[hdr_size..].to_vec();
            block_on_sync(dispatch_message(
                &hdr, &payload, state, &db, &outbox, peer, None,
            ))
            .unwrap()
            .unwrap();
        };
        run(&mut state, version_frame());
        run(&mut state, create_chan_frame(0x1234, "REF:MON")); // -> sid 1

        // EVENT_ADD via the shared register in HandOff mode: the initial value
        // lands in `outbox`, and we hold the live reader for the update.
        let add = event_add_double_frame(1, sub_id);
        let (hdr, hdr_size) =
            CaHeader::from_bytes_for_peer(&add, state.client_minor_version()).unwrap();
        let payload = add[hdr_size..].to_vec();
        let outcome = block_on_sync(register_subscription(
            &hdr,
            &payload,
            &state,
            &outbox,
            SubscriptionDelivery::HandOff,
            || 0usize,
            false,
        ))
        .expect("blockable")
        .expect("register ok");
        let r = match outcome {
            SubscriptionOutcome::HandedOff(r) => r,
            _ => panic!("HandOff must hand off the reader"),
        };
        let initial = drain_one_event_add(&mut drain);

        // Post the update, then frame it with the SAME `send_event` the event
        // thread uses.
        match &r.target {
            ChannelTarget::SimplePv(pv) => {
                block_on_sync(pv.set(EpicsValue::Double(update))).expect("blockable");
            }
            _ => panic!("REF:MON must be a simple PV"),
        }
        let mut reader = r.reader;
        let event = block_on_sync(reader.recv())
            .expect("blockable")
            .expect("an update event");
        crate::server::monitor::send_event(
            r.data_type,
            r.data_count,
            r.sub_id,
            &event,
            &outbox,
            r.long_string_mode,
            crate::server::tcp::ReplyContext {
                req_hdr: hdr,
                client_minor: r.client_minor,
            },
        )
        .expect("encode update");
        let update_frame = drain_one_event_add(&mut drain);
        (initial, update_frame)
    }

    /// (S1c-a-i) A monitor subscription over a real TCP socket delivers the
    /// initial value AND a later `db_post_events` update, each byte-identical
    /// to the shared register/`send_event` path (= the async server's bytes).
    #[test]
    fn monitor_delivers_initial_and_update_matching_shared_path() {
        let db = seed_db(&[("BLK:MON", EpicsValue::Double(42.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        c.write_all(&version_frame()).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:MON")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);
        assert_eq!(sid, 1, "first channel gets sid 1");

        // Subscribe → initial value.
        let sub_id = 0xAB;
        c.write_all(&event_add_double_frame(sid, sub_id)).unwrap();
        let initial = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD);
        assert_eq!(monitor_reply_value(&initial), 42.0, "initial monitor value");

        // db_post_events: the event thread delivers the update over the socket.
        let pv = simple_pv(&db, "BLK:MON");
        block_on_sync(pv.set(EpicsValue::Double(99.0))).expect("blockable");
        let update = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD);
        assert_eq!(monitor_reply_value(&update), 99.0, "posted monitor update");

        // Byte-for-byte parity against the shared register/send_event path.
        let (ref_initial, ref_update) = reference_monitor_frames(42.0, 99.0, sub_id);
        assert_eq!(
            initial, ref_initial,
            "initial monitor frame must match the shared path bytes"
        );
        assert_eq!(
            update, ref_update,
            "update monitor frame must match the shared path bytes"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    /// (S1c-a-ii) EVENTS_OFF gates monitor delivery; EVENTS_ON releases the
    /// held update. The event thread's readers consult the shared `EventUser`
    /// flow-control flag the dispatch thread flips — same as the async server.
    #[test]
    fn events_off_then_on_gates_delivery() {
        let db = seed_db(&[("BLK:GATE", EpicsValue::Double(1.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        c.write_all(&version_frame()).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:GATE")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

        let sub_id = 0xC1;
        c.write_all(&event_add_double_frame(sid, sub_id)).unwrap();
        let initial = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD);
        assert_eq!(monitor_reply_value(&initial), 1.0);

        // EVENTS_OFF, then a READ_NOTIFY barrier: reading its reply proves the
        // dispatch thread applied flow-control-on before we post.
        c.write_all(&events_off_frame()).unwrap();
        c.write_all(&read_notify_frame(sid, 0x1, DBR_DOUBLE_MON))
            .unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_READ_NOTIFY);

        // Post an update while flow control is on: it must be held, not sent.
        let pv = simple_pv(&db, "BLK:GATE");
        block_on_sync(pv.set(EpicsValue::Double(2.0))).expect("blockable");

        // A second READ_NOTIFY barrier: the dispatch thread has now cycled past
        // the post. Under EVENTS_OFF the ONLY frame that may follow is this
        // barrier's READ_NOTIFY reply — never a monitor update.
        c.write_all(&read_notify_frame(sid, 0x2, DBR_DOUBLE_MON))
            .unwrap();
        let barrier = read_one_frame(&mut c);
        assert_eq!(
            u16::from_be_bytes([barrier[0], barrier[1]]),
            CA_PROTO_READ_NOTIFY,
            "under EVENTS_OFF the next frame after a post is the READ barrier, not a monitor update"
        );
        // And nothing more is queued (the held update has not leaked).
        c.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut probe = [0u8; 1];
        let peeked = c.peek(&mut probe);
        assert!(
            peeked.as_ref().is_err_and(|e| is_read_timeout(e.kind())),
            "no monitor update may arrive while EVENTS_OFF, got {peeked:?}"
        );

        // EVENTS_ON releases the held update with the latest value.
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(&events_on_frame()).unwrap();
        let update = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD);
        assert_eq!(
            monitor_reply_value(&update),
            2.0,
            "held update delivered on EVENTS_ON"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    /// (S1c-a-iii) A client disconnect terminates its event thread cleanly: the
    /// producer subscriber is torn down (no leak) and the server keeps serving
    /// — a second subscriber still gets its own initial + update.
    #[test]
    fn client_disconnect_terminates_event_thread_cleanly() {
        let db = seed_db(&[("BLK:LEAK", EpicsValue::Double(7.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        // Client 1 subscribes and observes an update (event thread is live).
        {
            let mut c = TcpStream::connect(addr).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
            c.write_all(&version_frame()).unwrap();
            c.write_all(&create_chan_frame(1, "BLK:LEAK")).unwrap();
            let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
            let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);
            c.write_all(&event_add_double_frame(sid, 0x01)).unwrap();
            let _ = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD); // initial
            let pv = simple_pv(&db, "BLK:LEAK");
            block_on_sync(pv.set(EpicsValue::Double(8.0))).expect("blockable");
            let upd = read_until_cmmd(&mut c, CA_PROTO_EVENT_ADD);
            assert_eq!(monitor_reply_value(&upd), 8.0);
            // Drop `c`: the client thread's read loop exits; its teardown
            // removes the subscriber and joins the event thread.
        }

        // Teardown must remove the producer: the PV's subscriber list returns
        // to empty (proving the client thread ran cleanup, not leaked).
        let pv = simple_pv(&db, "BLK:LEAK");
        let mut cleared = false;
        for _ in 0..200 {
            if block_on_sync(pv.subscribers.lock())
                .expect("blockable")
                .is_empty()
            {
                cleared = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            cleared,
            "disconnect must remove the subscriber (event-task producer torn down)"
        );

        // The machinery is healthy afterwards: a second client subscribes and
        // gets its own initial + update (no leaked / wedged event thread).
        let mut c2 = TcpStream::connect(addr).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c2, CA_PROTO_VERSION);
        c2.write_all(&version_frame()).unwrap();
        c2.write_all(&create_chan_frame(2, "BLK:LEAK")).unwrap();
        let cc2 = read_until_cmmd(&mut c2, CA_PROTO_CREATE_CHAN);
        let sid2 = u32::from_be_bytes([cc2[12], cc2[13], cc2[14], cc2[15]]);
        c2.write_all(&event_add_double_frame(sid2, 0x02)).unwrap();
        let init2 = read_until_cmmd(&mut c2, CA_PROTO_EVENT_ADD);
        assert_eq!(
            monitor_reply_value(&init2),
            8.0,
            "second subscriber sees the current value"
        );
        let pv2 = simple_pv(&db, "BLK:LEAK");
        block_on_sync(pv2.set(EpicsValue::Double(9.0))).expect("blockable");
        let upd2 = read_until_cmmd(&mut c2, CA_PROTO_EVENT_ADD);
        assert_eq!(monitor_reply_value(&upd2), 9.0);
        drop(c2);

        server.shutdown();
        accept.join().unwrap();
    }

    // ---- S1c part (b): WRITE / WRITE_NOTIFY ----

    static ASYNC_ONCE_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

    /// A record whose FIRST `process()` goes async (the device round-trip C holds
    /// PACT for) and whose next pass completes synchronously — the minimal shape
    /// that drives a `CA_PROTO_WRITE_NOTIFY` put onto the
    /// [`WriteHeadOutcome::AsyncPending`] fork. `VAL` is `pp(TRUE)`, so a put to
    /// it processes the record; the put writes VAL, the process returns
    /// `AsyncPending`, and the completion fires later on `complete_async_record`
    /// (C `dbNotifyCompletion`). Mirrors `AsyncOnceRecord` in
    /// `epics-base-rs/tests/put_notify_defers_on_pact.rs`.
    struct AsyncOnceRecord {
        val: f64,
        pending: bool,
    }

    impl Record for AsyncOnceRecord {
        fn record_type(&self) -> &'static str {
            "asynconce_blk"
        }

        fn process(&mut self) -> CaResult<ProcessOutcome> {
            if self.pending {
                self.pending = false;
                Ok(ProcessOutcome {
                    result: RecordProcessResult::AsyncPending,
                    actions: Vec::new(),
                    device_did_compute: false,
                })
            } else {
                Ok(ProcessOutcome::complete())
            }
        }

        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }

        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match name {
                "VAL" => match value {
                    EpicsValue::Double(v) => {
                        self.val = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("VAL".into())),
                },
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }

        fn declared_fields(&self) -> &'static [FieldDesc] {
            ASYNC_ONCE_FIELDS
        }

        fn process_passive_fields(&self) -> &'static [&'static str] {
            &["VAL"]
        }
    }

    /// A one-record database whose sole record's first process goes async.
    fn seed_async_db(name: &str) -> Arc<PvDatabase> {
        let db = Arc::new(PvDatabase::new());
        block_on_sync(db.add_record(
            name,
            Box::new(AsyncOnceRecord {
                val: 0.0,
                pending: true,
            }),
        ))
        .expect("no runtime on test thread")
        .expect("add_record");
        db
    }

    /// A `CA_PROTO_WRITE_NOTIFY` (put-callback) frame for a scalar DBR_DOUBLE.
    fn write_notify_frame(sid: u32, ioid: u32, dbr_type: u16, value: f64) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
        h.data_type = dbr_type;
        h.count = 1;
        h.cid = sid;
        h.available = ioid;
        h.set_payload_size(8, 1, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&value.to_be_bytes());
        f
    }

    /// A deprecated fire-and-forget `CA_PROTO_WRITE` frame for a scalar
    /// DBR_DOUBLE (no ioid, no reply expected).
    fn plain_write_frame(sid: u32, dbr_type: u16, value: f64) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_WRITE);
        h.data_type = dbr_type;
        h.count = 1;
        h.cid = sid;
        h.available = 0;
        h.set_payload_size(8, 1, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&value.to_be_bytes());
        f
    }

    /// The DBR_DOUBLE scalar value in a READ_NOTIFY reply (payload after the
    /// 16-byte header).
    fn read_notify_value(frame: &[u8]) -> f64 {
        f64::from_be_bytes([
            frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22], frame[23],
        ])
    }

    /// The WRITE_NOTIFY completion reply the SHARED path produces for an async
    /// put on `AsyncOnceRecord`: `serve_write_head` (the sync head → the async
    /// fork) then `finish_write_notify` (the deferred reply), driven entirely
    /// in-process against its own db. The async server's WRITE_NOTIFY completion
    /// task runs these exact functions, so these are the async server's bytes —
    /// the byte-parity reference. Uses its own db so completing it here does not
    /// perturb the server under test.
    fn reference_write_notify_completion(put_val: f64, ioid: u32) -> Vec<u8> {
        let db = seed_async_db("REF:ASYW");
        let acf = new_acf();
        let mut state = ClientState::new(acf, 5064, db.clone());
        let peer: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        state.apply_connection_identity(peer, None, None);
        let (outbox, mut drain) = outbox::channel();

        let run = |state: &mut ClientState, frame: Vec<u8>| {
            let (hdr, hdr_size) =
                CaHeader::from_bytes_for_peer(&frame, state.client_minor_version()).unwrap();
            let payload = frame[hdr_size..].to_vec();
            block_on_sync(dispatch_message(
                &hdr, &payload, state, &db, &outbox, peer, None,
            ))
            .unwrap()
            .unwrap();
        };
        run(&mut state, version_frame());
        run(&mut state, create_chan_frame(0x1234, "REF:ASYW")); // -> sid 1

        // The shared head must fork to AsyncPending for a WRITE_NOTIFY on the
        // async record.
        let wn = write_notify_frame(1, ioid, DBR_DOUBLE_MON, put_val);
        let (hdr, hdr_size) =
            CaHeader::from_bytes_for_peer(&wn, state.client_minor_version()).unwrap();
        let payload = wn[hdr_size..].to_vec();
        let outcome = block_on_sync(serve_write_head(&hdr, &payload, &state, &db, &outbox))
            .expect("blockable")
            .expect("head ok");
        let mut p = match outcome {
            WriteHeadOutcome::AsyncPending(p) => p,
            WriteHeadOutcome::Done => {
                panic!("WRITE_NOTIFY on the async record must fork to AsyncPending")
            }
        };

        // The device round-trip finishes: C `dbNotifyCompletion` fires the
        // completion, then the shared tail sends the deferred reply.
        block_on_sync(db.complete_async_record("REF:ASYW"))
            .expect("blockable")
            .expect("completion");
        let final_status = match block_on_sync(&mut p.rx).expect("blockable") {
            Ok(()) => p.eca_status,
            Err(_) => crate::protocol::ECA_PUTFAIL,
        };
        crate::server::tcp::finish_write_notify(&mut p.trap_guard, final_status, &p.reply, &outbox)
            .expect("encode completion");

        while let Some(f) = drain.try_next() {
            if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_WRITE_NOTIFY {
                return f;
            }
        }
        panic!("reference produced no WRITE_NOTIFY completion frame");
    }

    /// (S1c-b-i) A WRITE_NOTIFY over a real TCP socket, on a record whose process
    /// cycle goes async, returns the completion reply ONLY after the record
    /// completes — and byte-identical to the shared head/finish path (= the async
    /// server's completion bytes).
    #[test]
    fn write_notify_async_completion_matches_reference() {
        let db = seed_async_db("BLK:ASYW");
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        c.write_all(&version_frame()).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:ASYW")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

        let ioid = 0x0BAD_F00D;
        c.write_all(&write_notify_frame(sid, ioid, DBR_DOUBLE_MON, 7.5))
            .unwrap();

        // No completion reply while the record's chain is still async.
        c.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut probe = [0u8; 1];
        let peeked = c.peek(&mut probe);
        assert!(
            peeked.as_ref().is_err_and(|e| is_read_timeout(e.kind())),
            "WRITE_NOTIFY must not reply before the async record completes, got {peeked:?}"
        );

        // The device round-trip finishes: the event thread ships the completion.
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        block_on_sync(db.complete_async_record("BLK:ASYW"))
            .unwrap()
            .unwrap();
        let reply = read_until_cmmd(&mut c, CA_PROTO_WRITE_NOTIFY);

        let ioid_echo = u32::from_be_bytes([reply[12], reply[13], reply[14], reply[15]]);
        assert_eq!(ioid_echo, ioid, "completion echoes the request ioid");

        // Byte-for-byte parity against the shared head/finish path.
        let reference = reference_write_notify_completion(7.5, ioid);
        assert_eq!(
            reply, reference,
            "blocking WRITE_NOTIFY completion must be byte-identical to the shared \
             serve_write_head/finish_write_notify path (= the async server's bytes)"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    /// (S1c-b-ii) A deprecated fire-and-forget CA_PROTO_WRITE performs the write
    /// with NO reply frame — the next frame after it is the READ_NOTIFY readback,
    /// which carries the written value.
    #[test]
    fn plain_write_performs_write_with_no_reply() {
        let db = seed_db(&[("BLK:PW", EpicsValue::Double(0.0))]);
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        c.write_all(&version_frame()).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:PW")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

        // Fire-and-forget WRITE, then a READ_NOTIFY readback pipelined behind it.
        c.write_all(&plain_write_frame(sid, DBR_DOUBLE_MON, 7.5))
            .unwrap();
        let ioid = 0x0000_5A5A;
        c.write_all(&read_notify_frame(sid, ioid, DBR_DOUBLE_MON))
            .unwrap();

        // The FIRST frame after the WRITE must be the READ_NOTIFY reply — a plain
        // WRITE emits no response of its own.
        let frame = read_one_frame(&mut c);
        assert_eq!(
            u16::from_be_bytes([frame[0], frame[1]]),
            CA_PROTO_READ_NOTIFY,
            "a fire-and-forget WRITE must emit no reply; the next frame is the READ_NOTIFY readback"
        );
        assert_eq!(
            u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]),
            ioid,
            "the readback echoes its own ioid"
        );
        assert_eq!(
            read_notify_value(&frame),
            7.5,
            "the plain WRITE reached the record"
        );

        server.shutdown();
        accept.join().unwrap();
    }

    /// (S1c-b-iii) While a WRITE_NOTIFY is pending (its record chain is still
    /// async), the dispatch thread stays responsive to further commands: a
    /// READ_NOTIFY issued before the record completes still gets its reply, and
    /// the WRITE_NOTIFY completion arrives only once the record settles.
    #[test]
    fn dispatch_thread_stays_responsive_while_write_notify_pending() {
        let db = seed_async_db("BLK:ASYR");
        let server =
            Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), new_acf()).unwrap());
        let addr = server.local_addr().unwrap();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = read_until_cmmd(&mut c, CA_PROTO_VERSION);
        c.write_all(&version_frame()).unwrap();
        c.write_all(&create_chan_frame(0x1234, "BLK:ASYR")).unwrap();
        let cc = read_until_cmmd(&mut c, CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

        // WRITE_NOTIFY that goes async: no completion yet (the record is PACT).
        let wn_ioid = 0x0000_9999;
        c.write_all(&write_notify_frame(sid, wn_ioid, DBR_DOUBLE_MON, 3.0))
            .unwrap();

        // The dispatch thread did NOT block on the put-callback: a READ_NOTIFY
        // issued while the WRITE_NOTIFY is still pending gets its reply. (The put
        // wrote VAL=3.0 before the process went async.)
        let rd_ioid = 0x0000_0001;
        c.write_all(&read_notify_frame(sid, rd_ioid, DBR_DOUBLE_MON))
            .unwrap();
        let rn = read_until_cmmd(&mut c, CA_PROTO_READ_NOTIFY);
        assert_eq!(
            u32::from_be_bytes([rn[12], rn[13], rn[14], rn[15]]),
            rd_ioid,
            "the dispatch thread served a READ_NOTIFY while the WRITE_NOTIFY was pending"
        );
        assert_eq!(
            read_notify_value(&rn),
            3.0,
            "the READ sees the value the pending put wrote before going async"
        );

        // Now the record settles: the WRITE_NOTIFY completion arrives.
        block_on_sync(db.complete_async_record("BLK:ASYR"))
            .unwrap()
            .unwrap();
        let wn = read_until_cmmd(&mut c, CA_PROTO_WRITE_NOTIFY);
        assert_eq!(
            u32::from_be_bytes([wn[12], wn[13], wn[14], wn[15]]),
            wn_ioid,
            "the deferred WRITE_NOTIFY completion echoes its request ioid"
        );

        server.shutdown();
        accept.join().unwrap();
    }
}
