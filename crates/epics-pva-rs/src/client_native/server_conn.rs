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
//! - **Heartbeat**: sends `ECHO_REQUEST` every `max(1, min(15, tcp_timeout×3/8))` s
//!   (pvxs clientconn.cpp:163-165); if no `last_rx` update has happened for
//!   `tcp_timeout`, declares the connection dead (pvxs clientconn.cpp:73-74).
//!
//! When any task exits (read EOF, write error, or heartbeat timeout) the
//! cancellation token fires and the connection is torn down. Channels
//! holding an `Arc<ServerConn>` observe the closed state via [`ServerConn::is_alive`]
//! and transition to "Reconnecting".

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout};
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

/// How often we send heartbeat ECHO_REQUEST.
///
/// Resolved at call time from `EPICS_PVA_CONN_TMO`: pvxs convention is
/// ECHO every `CONN_TMO / 2` so two heartbeats fit inside the timeout
/// window. Default 15 s when the env var is unset (CONN_TMO defaults
/// to 30 s).
pub fn heartbeat_interval() -> Duration {
    let configured = crate::config::env::conn_timeout_secs() as f64;
    Duration::from_secs_f64((configured / 2.0).max(1.0))
}

/// Maximum time we'll wait between any incoming bytes before declaring
/// the connection dead. pvxs effective timeout = configured × 4/3
/// (config.cpp:187 tmoScale) — without the margin a healthy client
/// races with its second ECHO. Floored at 2 s like pvxs `enforceTimeout`.
pub fn heartbeat_timeout() -> Duration {
    let configured = crate::config::env::conn_timeout_secs() as f64;
    Duration::from_secs_f64((configured * 4.0 / 3.0).max(2.0))
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
    pub byte_order: ByteOrder,
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
    /// Per-connection FieldDesc cache for 0xFD/0xFE wire markers.
    type_cache: Arc<Mutex<crate::pvdata::encode::TypeCache>>,
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

/// Type-erased read half. We accept either a plain TCP read half or a
/// TLS read half through the same code path.
type DynRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
/// Type-erased write half.
type DynWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

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
        let stream = timeout(conn.op_timeout, TcpStream::connect(target))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(PvaError::Io)?;
        stream.set_nodelay(true).ok();
        let (reader, writer) = stream.into_split();
        let reader: DynRead = Box::new(reader);
        let writer: DynWrite = Box::new(writer);
        // Plain `pva://` TCP — no TLS, so no server X.509 identity.
        Self::run_handshake_and_spawn(target, reader, writer, None, user, host, conn).await
    }

    /// Open a TLS-wrapped connection (`pvas://`).
    pub async fn connect_tls(
        target: SocketAddr,
        server_name: &str,
        tls: Arc<crate::auth::TlsClientConfig>,
        user: &str,
        host: &str,
        conn: ConnConfig,
    ) -> PvaResult<Arc<Self>> {
        let stream = timeout(conn.op_timeout, TcpStream::connect(target))
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
        let negotiated_auth = if auth_methods.iter().any(|m| m == "ca") {
            "ca"
        } else {
            "anonymous"
        };

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

        // Writer task
        let cancel_writer = cancel.clone();
        let alive_writer = alive.clone();
        let bytes_tx_writer = bytes_tx.clone();
        tokio::spawn(async move {
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
        let writer_tx_reader = writer_tx.clone();
        let order_reader = byte_order;
        tokio::spawn(async move {
            let mut buf = rx_buf;
            let mut chunk = vec![0u8; 4096];
            // P-G21: client-side segmented-message reassembly. Mirror
            // of the server-side state machine added in P-G20. pvxs
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
                            buf.extend_from_slice(&chunk[..n]);
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
                                    handle_control_frame(&frame, &writer_tx_reader, order_reader);
                                    continue;
                                }
                                // P-G21: segmentation gate (mirrors
                                // server-side P-G20 / pvxs conn.cpp:
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
                                seg_buf.extend_from_slice(&frame.payload);
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
                                );
                                route_frame(dispatch_frame, &by_ioid_reader, &by_cid_reader, &by_sid_close_reader, &ioid_to_sid_reader, &ioid_to_cmd_reader, &writer_tx_reader, order_reader, &cancel_reader);
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
        });

        // Heartbeat task
        let cancel_hb = cancel.clone();
        let alive_hb = alive.clone();
        let last_rx_hb = last_rx_nanos.clone();
        let writer_tx_hb = writer_tx.clone();
        let order_hb = byte_order;
        tokio::spawn(async move {
            // pvxs clientconn.cpp:163-165: echo interval = max(1, min(15, tcpTimeout * 3/8))
            // pvxs clientconn.cpp:73-74: socket inactivity timeout = tcpTimeout
            let hb_interval =
                Duration::from_secs_f64((tcp_timeout.as_secs_f64() * 3.0 / 8.0).clamp(1.0, 15.0));
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
                        // Send ECHO_REQUEST control message.
                        let h = PvaHeader::control(false, order_hb, ControlCommand::EchoRequest.code(), 0);
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
            byte_order,
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
            type_cache: Arc::new(Mutex::new(crate::pvdata::encode::TypeCache::new())),
        }))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
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

    /// Get a clone of the per-connection FieldDesc cache (Arc shared).
    /// Used by op decoders to resolve 0xFD/0xFE wire markers.
    pub fn type_cache(&self) -> Arc<Mutex<crate::pvdata::encode::TypeCache>> {
        self.type_cache.clone()
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
const DEFAULT_BUFFER_SIZE: u32 = 0x10000;
const DEFAULT_REGISTRY_SIZE: u16 = 32_767;

fn now_nanos() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

async fn read_handshake_init<R: tokio::io::AsyncRead + Unpin>(
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

async fn wait_for_validated<R: tokio::io::AsyncRead + Unpin>(
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
        // Same opt-in payload peek as the streaming reader (P-G8).
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
        rx_buf.extend_from_slice(&chunk[..n]);
    }
}

fn handle_control_frame(
    frame: &Frame,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
    order: ByteOrder,
) {
    if frame.header.command == ControlCommand::EchoRequest.code() {
        // Server pinged us — bounce back. Direct unbounded send: no
        // scheduler hop, mirrors the CA `DirectServerWriter` pattern.
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

#[allow(clippy::too_many_arguments)]
fn route_frame(
    frame: Frame,
    by_ioid: &Arc<DashMap<u32, IoidSlot>>,
    by_cid: &Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    by_sid_close: &Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
    ioid_to_sid: &Arc<DashMap<u32, u32>>,
    ioid_to_cmd: &Arc<DashMap<u32, u8>>,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
    order: ByteOrder,
    cancel: &CancellationToken,
) {
    let cmd = frame.header.command;

    // CMD_MESSAGE — log server diagnostic, don't route by IOID.
    if cmd == Command::Message.code() {
        log_server_message(&frame.payload, order);
        return;
    }

    // CMD_DESTROY_CHANNEL from server.
    if cmd == Command::DestroyChannel.code() {
        if let Some(sid) = peek_u32(&frame.payload, 0, order) {
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
        }
        return;
    }

    // CREATE_CHANNEL responses route by CID.
    if cmd == Command::CreateChannel.code() {
        if let Some(cid) = peek_u32(&frame.payload, 0, order) {
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
                return;
            }
            // no waiter at all — the caller timed out,
            // dropped its receiver, and CID was already evicted.
            // pvxs still sends DESTROY_CHANNEL so the server's
            // ChannelState is reaped. Pre-fix Rust silently dropped
            // the frame and left the server-side channel open until
            // TCP close.
            maybe_destroy_stale_create_channel(&frame, cid, writer_tx, order);
            return;
        }
    }

    // Application op responses (GET/PUT/MONITOR/RPC/GET_FIELD) route by IOID.
    if let Some(ioid) = peek_u32(&frame.payload, 0, order) {
        // verify the incoming frame's command matches the
        // command the IOID was opened with. Mirrors pvxs
        // `clientget.cpp:463-470` / `clientmon.cpp:570-579` per-op
        // command checks. A mismatch is protocol-fatal: cancel the
        // connection. Pre-fix Rust delivered any-cmd to the sink
        // matched by IOID alone — a buggy/malicious server could
        // satisfy a GET with a MONITOR-shaped frame.
        if let Some(expected) = ioid_to_cmd.get(&ioid).map(|r| *r.value()) {
            if expected != cmd {
                tracing::warn!(
                    ioid,
                    expected_cmd = expected,
                    actual_cmd = cmd,
                    "PVA client router: frame command mismatch for IOID, closing"
                );
                cancel.cancel();
                return;
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
    // Otherwise: drop silently. (Beacons/SearchResponse are handled
    // out-of-band by the search engine, not here.)
}

/// Log a server-side CMD_MESSAGE at the level matching its mtype.
/// Payload layout: `ioid:u32 + mtype:u8 + message:PVA-string`.
fn log_server_message(payload: &[u8], order: ByteOrder) {
    let mut cur = std::io::Cursor::new(payload);
    let Ok(ioid) = cur.get_u32(order) else { return };
    let Ok(mtype) = cur.get_u8() else { return };
    let msg = decode_string(&mut cur, order)
        .ok()
        .flatten()
        .unwrap_or_default();
    match mtype {
        x if x == MessageType::Info as u8 => {
            tracing::info!(ioid, msg, "server MESSAGE")
        }
        x if x == MessageType::Warning as u8 => {
            tracing::warn!(ioid, msg, "server MESSAGE")
        }
        x if x == MessageType::Error as u8 || x == MessageType::Fatal as u8 => {
            tracing::error!(ioid, msg, "server MESSAGE")
        }
        other => {
            tracing::warn!(ioid, mtype = other, msg, "server MESSAGE (unknown type)")
        }
    }
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

fn build_client_connection_validation(
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
    #[tokio::test]
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
                log_server_message(&payload, order);
            }
        }
    }

    #[test]
    fn log_server_message_handles_truncated_payload() {
        // Empty / too-short / no string body — must not panic.
        log_server_message(&[], ByteOrder::Little);
        log_server_message(&[0x01], ByteOrder::Little);
        log_server_message(&[0u8; 4], ByteOrder::Little); // ioid only, no mtype
        log_server_message(&[0u8; 5], ByteOrder::Little); // ioid + mtype but no string
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
            &writer_tx,
            order,
            &cancel,
        );
        assert!(flag.load(AtoOrd::Relaxed));
        assert!(!by_sid_close.contains_key(&sid));
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
            &writer_tx,
            ByteOrder::Little,
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
            &writer_tx,
            ByteOrder::Little,
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
            &writer_tx,
            ByteOrder::Little,
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
}
