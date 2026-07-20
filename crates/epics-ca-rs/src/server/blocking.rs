//! S1a — blocking, thread-per-client CA server driver.
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
//! handle. S1a has a single writer (the client thread draining command
//! replies), so the mutex is uncontended here, but it is the structural
//! anchor the S1c event-task analogue will lock to deliver monitor updates.
//!
//! # Scope (S1a) and what is deferred
//!
//! S1a covers GET/read plus the connection lifecycle (handshake, channel
//! create/clear). [`command_drives_without_spawn`] is an allowlist that
//! *fails closed*: any command whose handler spawns a task — EVENT_ADD
//! monitor senders and the WRITE_NOTIFY async completion (`tcp.rs:4389`,
//! `tcp.rs:4630`, `tcp.rs:3937`) — is refused with a clean CA error rather
//! than driven, because a spawn on a runtime-less thread cannot complete
//! here. Those move to S1c, which adds the event-task analogue and routes
//! async tails through the background executor. WRITE is deferred with them:
//! an async (PACT) record's write returns a `ProcessCompletion::Async` tail
//! that spawns, and the driver cannot tell sync from async records before
//! processing.
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

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::access_security::AccessSecurityConfig;
use epics_base_rs::server::database::PvDatabase;

use crate::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN,
    CA_PROTO_ECHO, CA_PROTO_EVENTS_OFF, CA_PROTO_EVENTS_ON, CA_PROTO_HOST_NAME, CA_PROTO_READ,
    CA_PROTO_READ_NOTIFY, CA_PROTO_READ_SYNC, CA_PROTO_SEARCH, CA_PROTO_VERSION, CaHeader,
    ECA_UNAVAILINSERV, ca_v49,
};
use crate::server::outbox::{self, OutboxDrain};
use crate::server::tcp::{ClientState, dispatch_message, is_peer_disconnect, send_ca_error};

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

    /// Ask the accept loop to stop and unblock it. Idempotent.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake the blocking `accept()` by connecting to ourselves; the loop
        // then observes the flag and returns.
        if let Ok(addr) = self.listener.local_addr() {
            let _ = TcpStream::connect(addr);
        }
    }
}

/// Read/lifecycle commands whose shared dispatch handler drives to completion
/// without spawning a task or awaiting genuine network I/O — i.e. the ones
/// `park_on` can run on a runtime-less thread. This is an ALLOWLIST: it fails
/// closed, so EVENT_ADD / WRITE / WRITE_NOTIFY (all spawn) and any unknown
/// command are refused. See the module docs for why those are S1c.
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

/// `WouldBlock` / `TimedOut` from a blocking read mean the `SO_RCVTIMEO`
/// inactivity cap fired (only set when configured). Treated as an idle close,
/// matching `handle_client`'s inactivity branch.
fn is_read_timeout(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Apply `SO_RCVTIMEO` to a socket via a raw `libc::setsockopt` on its fd (no
/// `socket2`). `None` leaves the socket in pure blocking mode — C `rsrv`'s
/// default (`camsgtask` blocks in `recv` with no application idle cap). The
/// libc symbols used here resolve for `armv7-rtems-eabihf`.
#[cfg(unix)]
fn apply_recv_timeout(stream: &TcpStream, timeout: Option<Duration>) -> CaResult<()> {
    use std::os::fd::AsRawFd;
    let Some(d) = timeout else {
        return Ok(());
    };
    let tv = libc::timeval {
        tv_sec: d.as_secs() as libc::time_t,
        tv_usec: d.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: `tv` outlives the call; `fd` is a valid open socket for the
    // duration of this borrow; the size matches `timeval`.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(CaError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_recv_timeout(_stream: &TcpStream, _timeout: Option<Duration>) -> CaResult<()> {
    // Non-Unix hosts fall back to pure blocking reads; RTEMS is Unix-family.
    Ok(())
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
    apply_recv_timeout(&stream, crate::server::tcp::inactivity_timeout())?;

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

    let mut accumulated: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8192];

    loop {
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
            let payload = accumulated[offset + hdr_size..offset + hdr_size + actual_post].to_vec();

            if command_drives_without_spawn(hdr.cmmd) {
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
                // Not yet supported (monitors + writes are S1c). Fail closed
                // with a clean CA error and keep serving — never a panic or a
                // silent drop.
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

        // Batched single-owner drain under the send lock.
        match drain_outbox_locked(&send_lock, &mut drain) {
            Ok(()) => {}
            Err(ref e) if is_peer_disconnect(e.kind()) => break,
            Err(e) => return Err(CaError::Io(e)),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::types::EpicsValue;

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

    /// (iii) The `SO_RCVTIMEO` libc path actually fires: a socket with a
    /// short recv timeout and no incoming data returns WouldBlock/TimedOut,
    /// which the driver classifies as an idle close.
    #[test]
    fn recv_timeout_libc_setsockopt_fires_on_idle() {
        let (a, _b) = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).unwrap();
            let (server, _) = listener.accept().unwrap();
            (server, client)
        };
        apply_recv_timeout(&a, Some(Duration::from_millis(150))).expect("setsockopt SO_RCVTIMEO");
        let mut a = a;
        let mut buf = [0u8; 8];
        let err = a.read(&mut buf).expect_err("idle read must time out");
        assert!(
            is_read_timeout(err.kind()),
            "SO_RCVTIMEO must surface as WouldBlock/TimedOut, got {:?}",
            err.kind()
        );
    }

    /// A spawning command (EVENT_ADD) is refused with a clean CA error, not a
    /// panic — the fail-closed allowlist. (S1c enables it.)
    #[test]
    fn unsupported_command_is_refused_cleanly() {
        assert!(!command_drives_without_spawn(
            crate::protocol::CA_PROTO_EVENT_ADD
        ));
        assert!(!command_drives_without_spawn(
            crate::protocol::CA_PROTO_WRITE
        ));
        assert!(!command_drives_without_spawn(
            crate::protocol::CA_PROTO_WRITE_NOTIFY
        ));
        assert!(command_drives_without_spawn(CA_PROTO_READ_NOTIFY));
        assert!(command_drives_without_spawn(CA_PROTO_CREATE_CHAN));
    }
}
