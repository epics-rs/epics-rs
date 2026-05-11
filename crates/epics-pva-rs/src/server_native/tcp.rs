//! TCP listener + per-connection handler.
//!
//! For each accepted client we spawn one task that:
//!
//! 1. Sends SET_BYTE_ORDER + CONNECTION_VALIDATION request
//! 2. Reads client's CONNECTION_VALIDATION response (auth)
//! 3. Sends CONNECTION_VALIDATED
//! 4. Loops reading channel ops (CREATE_CHANNEL / GET / PUT / MONITOR /
//!    GET_FIELD / DESTROY_REQUEST / DESTROY_CHANNEL).
//!
//! Channel state is kept per-connection (a `HashMap<sid, ChannelState>`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, error, warn};

use crate::client_native::decode::{Frame, try_parse_frame};
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    BitSet, ByteOrder, Command, ControlCommand, HeaderFlags, PVA_VERSION, PvaHeader, Status,
    WriteExt, encode_size_into, encode_string_into,
};
use crate::pvdata::encode::{
    EncodeTypeCache, decode_pv_field, decode_type_desc, encode_pv_field, encode_type_desc,
    encode_type_desc_cached,
};
use crate::pvdata::{FieldDesc, PvField};

use super::runtime::PvaServerConfig;
use super::source::DynSource;

static NEXT_SID: AtomicU32 = AtomicU32::new(1);
fn alloc_sid() -> u32 {
    NEXT_SID.fetch_add(1, Ordering::Relaxed)
}

struct PipelineOptions {
    enabled: bool,
    queue_size: u32,
}

/// Inspect a decoded pvRequest for `record._options.pipeline` and
/// `record._options.queueSize`. pvxs `Subscription` defaults to
/// `queueSize = 4` when pipeline is enabled; we follow.
fn monitor_pipeline_options(req: &PvField) -> Option<PipelineOptions> {
    use crate::pvdata::ScalarValue;
    let root = match req {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let record = root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))?;
    let record_s = match record {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let options = record_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))?;
    let opt_s = match options {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let pipeline_str = opt_s.fields.iter().find_map(|(k, v)| {
        (k == "pipeline").then_some(v).and_then(|v| match v {
            PvField::Scalar(ScalarValue::String(s)) => Some(s.as_str()),
            _ => None,
        })
    });
    let enabled = matches!(pipeline_str, Some("true") | Some("1"));
    let queue_size = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| {
            (k == "queueSize").then_some(v).and_then(|v| match v {
                PvField::Scalar(ScalarValue::String(s)) => s.parse::<u32>().ok(),
                PvField::Scalar(ScalarValue::Int(i)) => Some(*i as u32),
                PvField::Scalar(ScalarValue::Long(l)) => Some(*l as u32),
                _ => None,
            })
        })
        .unwrap_or(4);
    Some(PipelineOptions {
        enabled,
        queue_size: queue_size.max(1),
    })
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ChannelState {
    name: String,
    cid: u32,
    sid: u32,
    introspection: Option<FieldDesc>,
    /// ioid → (introspection negotiated for this op, kind)
    ops: HashMap<u32, OpState>,
}

/// Shared abort guard: when the last clone is dropped (HashMap removal,
/// connection end, ...), the spawned task is aborted automatically.
#[derive(Debug)]
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct OpState {
    intro: FieldDesc,
    kind: OpKind,
    /// For MONITOR ops: true once the subscriber task has been spawned.
    /// Subsequent START/pipeline-ack messages are no-ops.
    monitor_started: bool,
    /// Abort guard for the spawned MONITOR subscriber. Drop semantics
    /// (via `AbortOnDrop`) ensure the task is cancelled when the op is
    /// removed from the channel map (DestroyRequest), when the channel
    /// itself is removed (DestroyChannel), or when the connection ends.
    monitor_abort: Option<Arc<AbortOnDrop>>,
    /// Field mask derived from the client's pvRequest at INIT time.
    /// Drives the changed-bitset and partial-value encoding so the
    /// server only emits what was requested.
    mask: BitSet,
    /// Pipeline credit window (P-G11). pvxs `MonitorOp::window` —
    /// when pipeline mode is active, the server emits at most this
    /// many events before pausing until the client sends a
    /// MONITOR_ACK (subcmd 0x80) refilling the window. `None` when
    /// pipeline=false (no flow control on this op). Shared with the
    /// spawned subscriber via `Arc<AtomicU32>` so ACK messages can
    /// refill from the per-conn dispatch path.
    monitor_window: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Pulsed when `monitor_window` transitions from 0 → >0 so the
    /// subscriber loop can wake up and resume emission.
    monitor_window_notify: Option<Arc<tokio::sync::Notify>>,
    /// MONITOR pause flag (P-G28). pvxs subcmd `0x04` (without the
    /// `0x40` start bit) signals "stop emitting events but keep the
    /// op alive"; pvxs `Subscription::pause(true)` uses this. The
    /// subscriber task checks before emit and skips when `true`.
    /// Pulsed via the same notify as the credit window so the loop
    /// wakes on resume.
    monitor_paused: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OpKind {
    Get,
    Put,
    Monitor,
    Rpc,
}

/// Run the TCP listener forever. Backwards-compat wrapper that
/// drops per-peer stats — equivalent to calling
/// [`run_tcp_server_with_peers`] with an empty registry the caller
/// can never read.
pub async fn run_tcp_server(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
) -> PvaResult<()> {
    run_tcp_server_with_peers(
        source,
        bind_addr,
        config,
        crate::server_native::peers::PeerRegistry::new(),
    )
    .await
}

/// Run the TCP listener with an externally-shared
/// [`PeerRegistry`]. F-G7: lets [`crate::server_native::PvaServer::report`]
/// observe per-connection stats.
pub async fn run_tcp_server_with_peers(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let listener = TcpListener::bind(bind_addr).await.map_err(PvaError::Io)?;
    run_tcp_server_on_listener(source, listener, config, peers).await
}

/// Variant that takes a pre-bound [`TcpListener`]. Lets
/// [`crate::server_native::PvaServer::start`] perform the bind
/// synchronously (so the bound port is observable to callers) and
/// then hand the listener to the spawned accept task. Eliminates
/// the bind-race window that existed when the spawn-and-bind happened
/// inside the spawned task — concurrent isolated tests can no longer
/// have their picked-then-dropped ephemeral ports stolen by a peer.
pub async fn run_tcp_server_on_listener(
    source: DynSource,
    listener: TcpListener,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let bind_addr = listener.local_addr().map_err(PvaError::Io)?;
    debug!(?bind_addr, "TCP listener up");
    let active = Arc::new(AtomicUsize::new(0));

    let tls_acceptor = config
        .tls
        .as_ref()
        .map(|cfg| tokio_rustls::TlsAcceptor::from(cfg.config.clone()));

    // D-G1: track per-connection tasks in a JoinSet so they're
    // aborted as a unit when this accept-loop future is dropped (e.g.
    // PvaServer::stop() → tcp_handle.abort()). Without this, every
    // per-conn task ran detached and lingered until its internal
    // idle_timeout (~45s). The select! arm on `conn_tasks.join_next()`
    // also reaps completed tasks so the set doesn't accumulate
    // finished JoinHandles.
    let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        let accept_result = tokio::select! {
            biased;
            res = listener.accept() => res,
            // Drain finished connection tasks. Returns None when the
            // set is empty — that branch resolves immediately, but
            // `biased` makes the listener arm preferred so we never
            // starve incoming accepts.
            Some(_) = conn_tasks.join_next() => continue,
        };
        match accept_result {
            Ok((stream, peer)) => {
                if config.is_ignored_peer(peer) {
                    debug!(?peer, "rejecting connection: peer on ignore_addrs");
                    drop(stream);
                    continue;
                }
                let cur = active.fetch_add(1, Ordering::SeqCst);
                if cur >= config.max_connections {
                    active.fetch_sub(1, Ordering::SeqCst);
                    warn!(
                        ?peer,
                        "rejecting connection: max_connections={}", config.max_connections
                    );
                    drop(stream);
                    continue;
                }
                let src = source.clone();
                let cfg = config.clone();
                let active_dec = active.clone();
                let acceptor = tls_acceptor.clone();
                // F-G7: register this connection in the peer registry
                // so PvaServer::report() can surface it. Removed when
                // the connection task ends.
                let tls_in_use = acceptor.is_some();
                let peer_entry = crate::server_native::peers::PeerEntry::new(tls_in_use);
                peers.insert(peer, peer_entry.clone());
                let peers_for_task = peers.clone();
                conn_tasks.spawn(async move {
                    stream.set_nodelay(true).ok();
                    // Enable OS-level TCP keepalive so half-open connections
                    // (NAT timeout, dead client) are detected within ~30s
                    // even when the protocol-level Echo path can't fire
                    // (e.g. peer hasn't initialized control plane yet).
                    // Defence-in-depth on top of the heartbeat ECHO timer:
                    // pvxs itself does NOT set SO_KEEPALIVE — it relies on
                    // libevent's `bufferevent_set_timeouts` for inactivity
                    // detection. We add OS keepalive (CA-libca style) so a
                    // pre-handshake half-open peer still gets reaped even
                    // before the application timer arms.
                    {
                        let sock = socket2::SockRef::from(&stream);
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(std::time::Duration::from_secs(15))
                            .with_interval(std::time::Duration::from_secs(5));
                        let _ = sock.set_keepalive(true);
                        let _ = sock.set_tcp_keepalive(&keepalive);
                    }
                    let result = match acceptor {
                        // Round 8 P-G15: cap the TLS handshake — a peer
                        // that completes TCP but stalls during ClientHello
                        // would otherwise hold a `max_connections` slot
                        // until OS keepalive reaps it (~30s).
                        Some(a) => {
                            match tokio::time::timeout(cfg.tls_handshake_timeout, a.accept(stream))
                                .await
                            {
                                Ok(Ok(tls_stream)) => {
                                    let (r, w) = tokio::io::split(tls_stream);
                                    handle_connection_io(
                                        src,
                                        Box::new(r),
                                        Box::new(w),
                                        peer,
                                        cfg,
                                        peer_entry.clone(),
                                    )
                                    .await
                                }
                                Ok(Err(e)) => {
                                    debug!(?peer, "TLS handshake failed: {e}");
                                    Err(PvaError::Io(e))
                                }
                                Err(_) => {
                                    debug!(
                                        ?peer,
                                        timeout = ?cfg.tls_handshake_timeout,
                                        "TLS handshake timed out"
                                    );
                                    Err(PvaError::Protocol("TLS handshake timeout".into()))
                                }
                            }
                        }
                        None => {
                            let (r, w) = stream.into_split();
                            handle_connection_io(
                                src,
                                Box::new(r),
                                Box::new(w),
                                peer,
                                cfg,
                                peer_entry.clone(),
                            )
                            .await
                        }
                    };
                    if let Err(e) = result {
                        debug!(?peer, "connection ended: {e}");
                    }
                    active_dec.fetch_sub(1, Ordering::SeqCst);
                    // F-G7: drop the per-peer entry whether the
                    // connection ended cleanly or via I/O error.
                    peers_for_task.remove(peer);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Identity extracted from the client's CONNECTION_VALIDATION reply.
/// Mirrors pvxs `server::ClientCredentials` (serverconn.cpp:73-234) at
/// the wire-parse level — we don't currently feed it into ACF, but the
/// structured form is ready for future per-op authorisation hooks and
/// already lands in `tracing` for audit.
#[derive(Debug, Clone)]
pub struct ClientCredentials {
    /// Selected auth method ("anonymous" / "ca" / "x509" / ...).
    pub method: String,
    /// Account name (e.g., the `ca` auth's `user` field). Empty when
    /// the auth method does not carry one.
    pub account: String,
    /// Host name claim from the `ca` auth, when present. Informational
    /// only — never trust it for access decisions over the network
    /// hostname / mTLS-verified peer.
    pub host: String,
    /// Group / role claims advertised by the auth method. Populated
    /// by the `ca` method via [`crate::auth::posix_groups`] on the
    /// client side; on the server side the same list is parsed off
    /// the wire here. ACF rules of the form
    /// `R member group:operators` match against this set.
    pub roles: Vec<String>,
}

impl ClientCredentials {
    fn anonymous() -> Self {
        Self {
            method: "anonymous".into(),
            account: "anonymous".into(),
            host: String::new(),
            roles: Vec::new(),
        }
    }

    /// Format a one-line debug label for tracing / diagnostics.
    /// Mirrors pvxs `peerLabel()` (conn.cpp:50). Includes peer
    /// address, auth method, and account.
    pub fn peer_label(&self, peer: std::net::SocketAddr) -> String {
        if self.account.is_empty() {
            format!("{peer}/{}", self.method)
        } else {
            format!("{}@{peer}/{}", self.account, self.method)
        }
    }
}

/// Parse `CONNECTION_VALIDATION` reply payload (pvxs serverconn.cpp:200).
/// Layout: `buffer_size:u32 + intro_size:u16 + qos:u16 + method:String +
/// auth_type + auth_value`. Returns `None` on truncation; callers fall
/// back to anonymous credentials.
fn parse_client_credentials(frame: &Frame, order: ByteOrder) -> Option<ClientCredentials> {
    let mut cur = frame.cursor();
    let _buffer_size = cur.get_u32(order).ok()?;
    let _intro_size = cur.get_u16(order).ok()?;
    let _qos = cur.get_u16(order).ok()?;
    let method = crate::proto::decode_string(&mut cur, order)
        .ok()
        .flatten()
        .unwrap_or_default();
    if method.is_empty() {
        return Some(ClientCredentials::anonymous());
    }
    // Auth value: type descriptor + full value. We only care about
    // the `user` / `host` fields when method is "ca"; for any other
    // method the structured payload is opaque to us and we just store
    // the method name.
    let mut creds = ClientCredentials {
        method: method.clone(),
        account: String::new(),
        host: String::new(),
        roles: Vec::new(),
    };
    if let Ok(desc) = decode_type_desc(&mut cur, order) {
        if let Ok(PvField::Structure(s)) = decode_pv_field(&desc, &mut cur, order) {
            for (name, field) in &s.fields {
                match (name.as_str(), field) {
                    ("user", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                        creds.account = v.clone();
                    }
                    ("host", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                        creds.host = v.clone();
                    }
                    // pvxs ca-auth advertises POSIX groups as a
                    // string array under `groups` (or sometimes
                    // `roles`). Accept either name. Our PvField
                    // ScalarArray holds heterogeneous ScalarValue —
                    // we filter to the string-typed entries.
                    ("groups" | "roles", PvField::ScalarArray(arr)) => {
                        creds.roles = arr
                            .iter()
                            .filter_map(|sv| {
                                if let crate::pvdata::ScalarValue::String(s) = sv {
                                    Some(s.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();
                    }
                    _ => {}
                }
            }
        }
    }
    if creds.account.is_empty() {
        creds.account = method;
    }
    Some(creds)
}

/// Type-erased read/write halves so the same handler works for plain TCP
/// and TLS-wrapped streams.
type SrvRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type SrvWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
/// Per-connection write side. Producers (main read loop, heartbeat,
/// monitor subscribers) push fully-framed PVA messages into the
/// channel; a single dedicated writer task drains it in arrival order.
/// Replaces `Arc<Mutex<SrvWrite>>` so a slow client cannot block other
/// producers waiting for the lock. The channel is *bounded* —
/// `await`-style sends propagate backpressure all the way back to the
/// monitor subscribers / read loop, so memory cannot grow unbounded
/// when the client is slow. Errors on the write side drop the
/// receiver; subsequent sends fail and the read loop independently
/// observes the dead socket and tears down.
type SrvTx = tokio::sync::mpsc::Sender<Vec<u8>>;

async fn handle_connection_io(
    source: DynSource,
    mut reader: SrvRead,
    mut writer_raw: SrvWrite,
    peer: SocketAddr,
    config: PvaServerConfig,
    peer_entry: Arc<crate::server_native::peers::PeerEntry>,
) -> PvaResult<()> {
    let op_timeout = config.op_timeout;
    let idle_timeout = config.idle_timeout;

    // Spawn the dedicated writer task. All emit sites push framed bytes
    // into `tx`; the task drains and writes serially. Two failure
    // modes are detected:
    // 1. Hard I/O error — the underlying socket returned an error.
    //    `write_all` returns Err; we exit and the receiver closes,
    //    so subsequent `tx.send(...)` calls fail immediately.
    // 2. Stuck client — the kernel send buffer is full because the
    //    peer stopped reading. `write_all` returns Pending forever
    //    on a non-blocking socket; without a guard the writer task
    //    would hang and back-pressure both the heartbeat and the
    //    read-side dispatcher (since both push into the same mpsc).
    //    We wrap `write_all` in `tokio::time::timeout(send_timeout)`
    //    so a stalled write breaks the task, closes the mpsc, and
    //    fails fast. Mirrors the parallel guard in `epics-ca-rs`'s
    //    server-side dispatch wrap (the CA G1 audit fix).
    let send_tmo = config.send_timeout;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(config.write_queue_depth);
    let writer_peer = peer;
    let peer_entry_writer = peer_entry.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match tokio::time::timeout(send_tmo, writer_raw.write_all(&frame)).await {
                Ok(Ok(())) => {
                    // F-G7: bytes_out counter for PvaServer::report().
                    peer_entry_writer.touch_tx(frame.len());
                }
                Ok(Err(e)) => {
                    debug!(peer = ?writer_peer, error = %e, "writer task: TCP write failed, dropping connection");
                    break;
                }
                Err(_) => {
                    warn!(
                        peer = ?writer_peer,
                        timeout_secs = send_tmo.as_secs_f64(),
                        "writer task: send timeout (stuck client?), dropping connection"
                    );
                    break;
                }
            }
        }
    });
    // P-G18: abort the writer + heartbeat tasks the moment the read
    // loop returns. Without this, both linger up to `idle_timeout`
    // (default 45s) emitting ECHOes into a channel nobody is reading
    // and holding the writer half of the (now-disconnected) socket.
    // pvxs uses libevent-driven cleanup that shuts everything in one
    // pass; we rely on tokio JoinHandle::abort() via AbortOnDrop.
    let _writer_guard = AbortOnDrop(writer_task.abort_handle());

    // Track per-connection liveness for the idle-timeout watchdog and the
    // server-side echo heartbeat task.
    let last_rx = Arc::new(AtomicU64::new(now_nanos()));

    // Spawn server-side heartbeat: send ECHO_REQUEST every 15 s; close if
    // we've been idle for `idle_timeout`.
    let last_rx_hb = last_rx.clone();
    let tx_hb = tx.clone();
    let order_hb = config.wire_byte_order;
    let hb_handle = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(15));
        tick.tick().await;
        loop {
            tick.tick().await;
            let last = last_rx_hb.load(Ordering::SeqCst);
            let elapsed = now_nanos().saturating_sub(last);
            if Duration::from_nanos(elapsed) > idle_timeout {
                warn!(?peer, "PVA client idle > {idle_timeout:?}; closing");
                break;
            }
            let h = PvaHeader::control(true, order_hb, ControlCommand::EchoRequest.code(), 0);
            let mut buf = Vec::with_capacity(8);
            h.write_into(&mut buf);
            if tx_hb.send(buf).await.is_err() {
                break;
            }
        }
    });
    let _hb_guard = AbortOnDrop(hb_handle.abort_handle());

    let order = config.wire_byte_order;

    // Step 1: send SET_BYTE_ORDER (control message). Per pvxs, the byte order
    // we want to use is encoded in the control header's flag bit 7.
    let set_bo = {
        let mut buf = Vec::with_capacity(8);
        let h = PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0);
        h.write_into(&mut buf);
        buf
    };
    let _ = tx.send(set_bo).await;

    // Step 2: send CONNECTION_VALIDATION request (server → client).
    let val_req = build_server_connection_validation(order, 87_040, 32_767, &["ca", "anonymous"]);
    let _ = tx.send(val_req).await;

    // Step 3+: drive the read loop.
    let mut rx_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut channels: HashMap<u32, ChannelState> = HashMap::new();
    let mut handshake_complete = false;
    // Client identity carried for the rest of the connection lifetime.
    // Extracted from the CONNECTION_VALIDATION reply; falls back to
    // anonymous when the client either skips the exchange (some legacy
    // clients) or sends an unparseable payload. Available for future
    // per-op authorisation hooks; today only logged at handshake time.
    let mut cred = ClientCredentials::anonymous();
    // Per-connection emit-side TypeStore. Only consulted when
    // `config.emit_type_cache` is true (off by default for pvAccessCPP
    // compatibility — that client does not parse 0xFD/0xFE markers).
    let mut encode_type_cache = crate::pvdata::encode::EncodeTypeCache::new();

    let max_msg_size = config.max_message_size;
    // P-G20: segmented-message reassembly state. pvxs conn.cpp:228-291
    // accumulates SegFirst..SegMiddle..SegLast bodies into `segBuf`
    // before dispatching. Without this, our server would treat every
    // segment as a fresh message, decode garbage, and likely return
    // a Decode error mid-payload. Sites that put bulk values
    // (NTTable, large NTNDArray, multi-MiB NTScalarArray) over PVA
    // hit segmented frames whenever the message exceeds the peer's
    // buffer-size hint negotiated in CONNECTION_VALIDATION.
    let mut seg_buf: Vec<u8> = Vec::new();
    let mut seg_cmd: u8 = 0;
    let mut expect_seg = false;
    loop {
        // C-G2: if the writer task has died (send_timeout fired,
        // panic, etc.) the outbound mpsc is closed. Every subsequent
        // `let _ = tx.send(...).await` in the dispatch path silently
        // discards its frame and the client never sees the response,
        // but the read loop would otherwise keep accumulating
        // per-IOID state until `op_timeout` (default 64,000 s) or
        // `idle_timeout` (45 s) tore the connection down. Detect
        // the writer death here and unwind immediately so the
        // channels HashMap drop fires its AbortOnDrop chain and the
        // peer's connection slot is released within ms instead of
        // ~30-45 s.
        if tx.is_closed() {
            return Ok(());
        }
        let frame = read_frame(&mut reader, &mut rx_buf, op_timeout, max_msg_size).await?;
        // F-G7: bytes_in counter (header + payload). Drives
        // PvaServer::report() throughput diagnostics.
        peer_entry.touch_rx(PvaHeader::SIZE + frame.payload.len());
        last_rx.store(now_nanos(), Ordering::SeqCst);
        if frame.header.flags.is_control() {
            // Handle echo etc., otherwise ignore.
            if frame.header.command == ControlCommand::EchoRequest.code() {
                let mut buf = Vec::new();
                let h = PvaHeader::control(
                    true,
                    order,
                    ControlCommand::EchoResponse.code(),
                    frame.header.payload_length,
                );
                h.write_into(&mut buf);
                let _ = tx.send(buf).await;
            }
            continue;
        }

        // P-G20: segmentation gate. Mirrors pvxs conn.cpp:228-244.
        //   continuation = SegLast bit set (true for mid OR last)
        //   * Violation when (continuation XOR expect_seg) — peer
        //     interleaved a fresh first/unsegmented frame inside a
        //     pending segmented message, OR sent a continuation when
        //     none was pending.
        //   * Violation when continuation && cmd != saved_cmd.
        // Either case → drop connection (decode would be undefined).
        let raw_seg = frame.header.flags.0 & HeaderFlags::SEGMENT_MASK;
        let continuation = raw_seg & HeaderFlags::SEGMENT_LAST != 0;
        if continuation ^ expect_seg || (continuation && frame.header.command != seg_cmd) {
            return Err(PvaError::Protocol(format!(
                "PVA segmentation violation: expect_seg={} continuation={} cmd 0x{:02x} vs saved 0x{:02x}",
                expect_seg, continuation, frame.header.command, seg_cmd
            )));
        }
        if raw_seg == 0 || raw_seg == HeaderFlags::SEGMENT_FIRST {
            // Start of a new logical message — reset the accumulator
            // (in unsegmented case both reset and dispatch happen
            // below).
            expect_seg = true;
            seg_cmd = frame.header.command;
            seg_buf.clear();
        }
        // Cap reassembly at max_msg_size. read_frame already enforces
        // it per-frame; without this an adversary streams SegFirst →
        // SegMiddle … forever, growing seg_buf without bound.
        if seg_buf.len().saturating_add(frame.payload.len()) > max_msg_size {
            return Err(PvaError::Protocol(format!(
                "segmented PVA message exceeds max_message_size ({} > {})",
                seg_buf.len() + frame.payload.len(),
                max_msg_size
            )));
        }
        seg_buf.extend_from_slice(&frame.payload);
        if raw_seg != 0 && raw_seg != HeaderFlags::SEGMENT_LAST {
            // SegFirst (with following segments) or SegMiddle: keep
            // accumulating, do not dispatch yet.
            continue;
        }
        // Reaching here means: unsegmented (raw_seg==0) OR SegLast.
        expect_seg = false;
        // Build a synthetic Frame whose payload is the reassembled
        // body; dispatch path inspects only `header.command` and
        // `payload`, plus byte-order via `frame.order()`.
        let frame = if raw_seg == 0 {
            frame
        } else {
            Frame {
                header: PvaHeader {
                    version: frame.header.version,
                    flags: HeaderFlags::new(false, false, order),
                    command: seg_cmd,
                    payload_length: seg_buf.len() as u32,
                },
                payload: std::mem::take(&mut seg_buf),
            }
        };

        // Pre-handshake: only CONNECTION_VALIDATION (1) is meaningful; client
        // replies with its buffer/registry/qos/auth payload. We accept any
        // and respond CONNECTION_VALIDATED.
        if !handshake_complete {
            if frame.header.command == Command::ConnectionValidation.code() {
                // Parse the client's auth payload: skip buffer_size (u32),
                // introspection_size (u16), qos (u16); read selected method
                // (string); when method == "ca", read the type+value of the
                // auth Value and pull out the `user` / `host` fields. Pure
                // metadata for audit/logging — we still respond OK either
                // way (matches pvxs serverconn.cpp:200-234, which also
                // doesn't gate the ack on auth content).
                cred = parse_client_credentials(&frame, order).unwrap_or(cred);
                debug!(?peer, method = %cred.method, account = %cred.account,
                    roles = ?cred.roles, "PVA client credentials");
                let mut payload = Vec::new();
                Status::ok().write_into(order, &mut payload);
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::ConnectionValidated.code(),
                    payload.len() as u32,
                );
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx.send(buf).await;
                handshake_complete = true;
                // Fire user-installed `auth_complete` hook (pvxs
                // serverconn.cpp:181 parity) once we've accepted the
                // peer's identity claim. Hook signature mirrors pvxs
                // — peer addr + credentials snapshot. ACF
                // integration goes here.
                if let Some(hook) = config.auth_complete.as_ref() {
                    hook(peer, &cred);
                }
                continue;
            } else {
                // Some clients send CREATE_CHANNEL right after SET_BYTE_ORDER
                // skipping a fresh CONNECTION_VALIDATION exchange — accept.
                handshake_complete = true;
            }
        }

        // Application messages
        match Command::from_code(frame.header.command) {
            Some(Command::CreateChannel) => {
                if channels.len() >= config.max_channels_per_connection {
                    warn!(
                        ?peer,
                        "rejecting CREATE_CHANNEL: per-connection limit reached"
                    );
                    // Reject by sending an error CreateChannel response with cid=u32::MAX.
                    let mut payload = Vec::new();
                    payload.put_u32(u32::MAX, order);
                    payload.put_u32(0u32, order);
                    Status::error("max channels per connection reached".to_string())
                        .write_into(order, &mut payload);
                    let h = PvaHeader::application(
                        true,
                        order,
                        Command::CreateChannel.code(),
                        payload.len() as u32,
                    );
                    let mut buf = Vec::new();
                    h.write_into(&mut buf);
                    buf.extend_from_slice(&payload);
                    let _ = tx.send(buf).await;
                    continue;
                }
                let before = channels.len();
                handle_create_channel(&source, &frame, &tx, &mut channels, order).await?;
                // F-G7: track channel-add success via the HashMap
                // delta. Avoids threading peer_entry into every
                // handler; the size check is O(1) and the rare
                // duplicate-cid path naturally evaluates to no-op.
                if channels.len() > before {
                    peer_entry.channel_added();
                }
            }
            Some(Command::DestroyChannel) => {
                let before = channels.len();
                handle_destroy_channel(&frame, &tx, &mut channels, order).await?;
                if channels.len() < before {
                    peer_entry.channel_removed();
                }
            }
            Some(Command::Get) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Get,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Put) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Put,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Monitor) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Monitor,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Rpc) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Rpc,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::GetField) => {
                handle_get_field(&source, &frame, &tx, &channels, order).await?;
            }
            Some(Command::DestroyRequest) => {
                handle_destroy_request(&frame, &mut channels, order);
            }
            Some(Command::CancelRequest) => {
                handle_cancel_request(&frame, &mut channels, order);
            }
            Some(Command::Message) => {
                handle_message(&frame, order, &peer);
            }
            Some(Command::PutGet) => {
                // I-1 (Round I): atomic put-then-get. The pvxs
                // wire spec defines this as a separate command but
                // pvxs itself rarely implements it — production
                // sites use `record[process=true]` in the PUT
                // pvRequest options instead. We respond with a
                // typed "not supported" error so legacy clients
                // get a clean failure message rather than a
                // protocol parse error or silent timeout.
                handle_unsupported_op(&frame, &tx, Command::PutGet, "PUT_GET", order).await;
            }
            Some(Command::Process) => {
                // I-1: same handling as PutGet — modern PVA uses
                // pvRequest options, not the wire-level PROCESS
                // command. Respond clean.
                handle_unsupported_op(&frame, &tx, Command::Process, "PROCESS", order).await;
            }
            Some(Command::OriginTag) => {
                // I-5: pvxs origin-tag is an optional payload for
                // tracing/debugging that the spec lets servers
                // ignore. We log at debug level and carry on so a
                // client that sends one still works.
                debug!(
                    peer = ?peer,
                    bytes = frame.payload.len(),
                    "OriginTag received (silently consumed)"
                );
            }
            Some(Command::AclChange) => {
                // I-5: AclChange is a server → client push that
                // pvxs / pvAccessCPP servers emit when access
                // rights for a channel change. We don't yet wire
                // up server-side ACF mutation events, so receiving
                // one as a server (which shouldn't happen) is
                // logged-and-ignored. As a client we'd react in
                // the read loop.
                debug!(
                    peer = ?peer,
                    "AclChange received as server (unexpected); ignoring"
                );
            }
            Some(Command::MultipleData) => {
                // I-5: MultipleData was a never-really-deployed
                // batch monitor delivery format. pvxs decodes it
                // but our client/server only emit single-data
                // monitor frames. Server-side receipt is
                // inappropriate — log and drop.
                debug!(
                    peer = ?peer,
                    "MultipleData received as server (unexpected); ignoring"
                );
            }
            Some(Command::Echo) => {
                // Echo back the same frame.
                let mut buf = Vec::new();
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::Echo.code(),
                    frame.payload.len() as u32,
                );
                h.write_into(&mut buf);
                buf.extend_from_slice(&frame.payload);
                let _ = tx.send(buf).await;
            }
            _ => {
                // Unhandled — keep going.
            }
        }
    }
}

/// I-1: respond to a wire command we don't implement with a typed
/// error frame instead of letting the client time out. The
/// response shape mirrors the standard op error response so a
/// pvxs / pvAccessJava client surfaces the message verbatim
/// instead of a protocol violation.
async fn handle_unsupported_op(
    frame: &Frame,
    tx: &SrvTx,
    cmd: Command,
    label: &str,
    order: ByteOrder,
) {
    use crate::proto::WriteExt;
    let mut cur = frame.cursor();
    let _sid = cur.get_u32(order).unwrap_or(0);
    let ioid = cur.get_u32(order).unwrap_or(0);
    let _subcmd = cur.get_u8().unwrap_or(0);

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00); // subcmd 0 = data response
    let status = Status::error(format!("{label} not supported by this server"));
    status.write_into(order, &mut payload);

    let header = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
    let mut buf = Vec::new();
    header.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_msg_size: usize,
) -> PvaResult<Frame> {
    loop {
        if let Some((frame, n)) = try_parse_frame(rx_buf)? {
            rx_buf.drain(..n);
            return Ok(frame);
        }
        // Peek the header length once we have 8 bytes — if the peer
        // claimed a payload larger than `max_msg_size`, drop the
        // connection before growing rx_buf any further. Without this
        // a malicious header announcing 4 GiB would force us to
        // OOM-loop here. pvxs enforces the same cap implicitly via
        // libevent's evbuffer_setwatermark; we do it explicitly.
        if rx_buf.len() >= PvaHeader::SIZE {
            if let Ok(hdr) = PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[..])) {
                if !hdr.flags.is_control() && hdr.payload_length as usize > max_msg_size {
                    return Err(PvaError::Protocol(format!(
                        "inbound payload {} exceeds max_message_size {}",
                        hdr.payload_length, max_msg_size
                    )));
                }
            }
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(op_timeout, reader.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(PvaError::Io(e)),
            Err(_) => return Err(PvaError::Timeout),
        };
        if n == 0 {
            return Err(PvaError::Protocol("client closed".into()));
        }
        rx_buf.extend_from_slice(&chunk[..n]);
    }
}

/// Build a server-side CONNECTION_VALIDATION request (cmd=1, server direction).
///
/// Wire layout (8-byte header + this payload):
///
/// ```text
/// u32 buffer_size
/// u16 introspection_registry_size
/// Size n
/// n × String   (auth method names)
/// ```
fn build_server_connection_validation(
    order: ByteOrder,
    buffer_size: u32,
    registry_size: u16,
    auth_methods: &[&str],
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(buffer_size, order);
    payload.put_u16(registry_size, order);
    encode_size_into(auth_methods.len() as u32, order, &mut payload);
    for m in auth_methods {
        encode_string_into(m, order, &mut payload);
    }
    let h = PvaHeader::application(
        true,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut out = Vec::new();
    h.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

async fn handle_create_channel(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let _count = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let cid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let name = crate::proto::decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?
        .unwrap_or_default();

    if !source.has_pv(&name).await {
        let mut payload = Vec::new();
        payload.put_u32(cid, order);
        payload.put_u32(0u32, order); // sid (placeholder)
        Status::error(format!("unknown PV: {name}")).write_into(order, &mut payload);
        let h = PvaHeader::application(
            true,
            order,
            Command::CreateChannel.code(),
            payload.len() as u32,
        );
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    let sid = alloc_sid();
    let intro = source.get_introspection(&name).await;
    channels.insert(
        sid,
        ChannelState {
            name: name.clone(),
            cid,
            sid,
            introspection: intro,
            ops: HashMap::new(),
        },
    );

    let mut payload = Vec::new();
    payload.put_u32(cid, order);
    payload.put_u32(sid, order);
    Status::ok().write_into(order, &mut payload);
    // pvxs serverchan.cpp:349-351 emits `cid + sid + status` only —
    // no access_rights field follows.
    let h = PvaHeader::application(
        true,
        order,
        Command::CreateChannel.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

async fn handle_destroy_channel(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let cid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    // Removing the channel drops every OpState in `ops`, which drops
    // each `monitor_abort: Option<Arc<AbortOnDrop>>` and cancels the
    // associated subscriber task — preventing orphaned spawns from
    // holding the source's broadcast subscription.
    channels.remove(&sid);
    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(cid, order);
    let h = PvaHeader::application(
        true,
        order,
        Command::DestroyChannel.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

/// Handle CANCEL_REQUEST (cmd 21). pvxs serverconn.cpp:262 — moves the op
/// from Executing back to Idle without freeing it, so the client can
/// re-trigger (e.g., re-START a paused monitor). For our model, that
/// means: stop the running subscriber but keep the OpState so a fresh
/// MONITOR START can re-spawn it.
fn handle_cancel_request(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) {
    let mut cur = frame.cursor();
    let Ok(sid) = cur.get_u32(order) else { return };
    let Ok(ioid) = cur.get_u32(order) else { return };
    if let Some(ch) = channels.get_mut(&sid) {
        if let Some(op) = ch.ops.get_mut(&ioid) {
            // Drop the abort guard → subscriber task aborts.
            op.monitor_abort = None;
            op.monitor_started = false;
        }
    }
}

/// Handle MESSAGE (cmd 18). pvxs serverconn.cpp:323 — clients send
/// log messages tagged with severity (Info/Warning/Error/Fatal). We
/// surface them through the `tracing` crate at the matching level.
fn handle_message(frame: &Frame, order: ByteOrder, peer: &SocketAddr) {
    let mut cur = frame.cursor();
    let Ok(ioid) = cur.get_u32(order) else { return };
    let Ok(mtype) = cur.get_u8() else { return };
    let msg = match crate::proto::decode_string(&mut cur, order) {
        Ok(Some(s)) => s,
        _ => String::new(),
    };
    match mtype {
        0 => debug!(?peer, ioid, message = %msg, "client info"),
        1 => warn!(?peer, ioid, message = %msg, "client warning"),
        2 | 3 => error!(?peer, ioid, message = %msg, "client error"),
        _ => debug!(?peer, ioid, mtype, message = %msg, "client message (unknown type)"),
    }
}

fn handle_destroy_request(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) {
    let mut cur = frame.cursor();
    let Ok(sid) = cur.get_u32(order) else { return };
    let Ok(ioid) = cur.get_u32(order) else { return };
    if let Some(ch) = channels.get_mut(&sid) {
        // Removing the op drops `monitor_abort: Option<Arc<AbortOnDrop>>`.
        // Once the last clone is dropped, the subscriber task aborts.
        ch.ops.remove(&ioid);
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_op(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    kind: OpKind,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            // Send error.
            send_op_error(tx, kind, ioid, "unknown channel sid", order).await?;
            return Ok(());
        }
    };

    if subcmd & 0x08 != 0 {
        // A-G1: per-channel concurrent-op cap — refuse fresh INITs
        // once the channel's `ops` map hits the configured ceiling
        // so a malicious peer can't accumulate IOID state forever
        // by sending INIT … INIT … without ever issuing DESTROY.
        // Existing IOIDs (re-INIT on a known IOID) are allowed to
        // proceed — the insert below replaces the entry without
        // growing the map.
        if !ch.ops.contains_key(&ioid) && ch.ops.len() >= config.max_ops_per_channel {
            send_op_error(tx, kind, ioid, "max ops per channel exceeded", order).await?;
            return Ok(());
        }
        // INIT — read pvRequest (`type + full value` per pvxs
        // clientget.cpp:351-352) and translate it to a field mask the
        // emit side will consult.
        let intro = ch.introspection.clone().unwrap_or(FieldDesc::Variant);

        let req_desc = decode_type_desc(&mut cur, order).ok();
        let req_value = req_desc
            .as_ref()
            .and_then(|d| decode_pv_field(d, &mut cur, order).ok());
        let mask = req_desc
            .as_ref()
            .and_then(|d| crate::pv_request::request_to_mask(&intro, d).ok())
            .unwrap_or_else(|| BitSet::all_set(intro.total_bits()));

        // Pipeline flow control is opt-in via pvRequest:
        // `record[pipeline=true,queueSize=N]`. pvxs only enables the
        // credit/ACK window when the client explicitly sets it;
        // applying it unconditionally produced a 5-event-then-stall
        // bug for default `pvmonitor` callers (initial snapshot + 4
        // window credits). Without pipeline=true we don't gate the
        // emit loop — mpsc backpressure remains the only limiter.
        let pipeline_opt = req_value
            .as_ref()
            .and_then(monitor_pipeline_options)
            .filter(|o| o.enabled);
        let (monitor_window, monitor_window_notify) = if kind == OpKind::Monitor
            && let Some(opt) = pipeline_opt
        {
            (
                Some(Arc::new(std::sync::atomic::AtomicU32::new(opt.queue_size))),
                Some(Arc::new(tokio::sync::Notify::new())),
            )
        } else {
            (None, None)
        };

        ch.ops.insert(
            ioid,
            OpState {
                intro: intro.clone(),
                kind,
                monitor_started: false,
                monitor_abort: None,
                mask,
                monitor_window,
                monitor_window_notify,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );

        // Build INIT response: ioid + subcmd + status + introspection
        let cmd = match kind {
            OpKind::Get => Command::Get,
            OpKind::Put => Command::Put,
            OpKind::Monitor => Command::Monitor,
            OpKind::Rpc => Command::Rpc,
        };

        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        Status::ok().write_into(order, &mut payload);
        // RPC INIT carries no type descriptor (pvxs serverget.cpp:97 —
        // `if (cmd != CMD_RPC) to_wire(R, type)`). GET/PUT/MONITOR INIT
        // emits the introspection — inline by default; with
        // `config.emit_type_cache`, repeated descriptors collapse into
        // 3-byte 0xFE references via the per-connection TypeStore.
        if !matches!(kind, OpKind::Rpc) {
            if config.emit_type_cache {
                encode_type_desc_cached(&intro, order, encode_cache, &mut payload);
            } else {
                encode_type_desc(&intro, order, &mut payload);
            }
        }
        let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    // Data phase
    let op = ch.ops.get(&ioid).cloned();
    let (intro, mask) = match op {
        Some(o) => (o.intro, o.mask),
        None => {
            send_op_error(tx, kind, ioid, "operation not initialised", order).await?;
            return Ok(());
        }
    };

    match kind {
        OpKind::Get => {
            // Round 41: type-state ACF gate. The wire layer mints the
            // [`AccessChecked`] token via the source's per-instance
            // [`AccessGate`]; the source's `get_value_checked` then
            // refuses to proceed when the level is `NoAccess`. The
            // token is unforgeable outside the gate, so adding a new
            // wire op without going through the gate is a compile
            // error against the trait method signature.
            let ctx = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
            };
            let checked = source
                .access_gate()
                .check(&ch.name, &ctx.host, &ctx.account, &ctx.method, "")
                .await;
            let value = match source.get_value_checked(checked, ctx).await {
                Some(v) => v,
                None => {
                    send_op_error(tx, OpKind::Get, ioid, "PV not found", order).await?;
                    return Ok(());
                }
            };
            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            payload.put_u8(0x00);
            Status::ok().write_into(order, &mut payload);
            // Emit only the fields the client's pvRequest selected.
            mask.write_into(order, &mut payload);
            crate::pvdata::encode::encode_pv_field_with_bitset(
                &value,
                &intro,
                &mask,
                0,
                order,
                &mut payload,
            );
            let h = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        OpKind::Put => {
            // Read bitset (which fields client is putting) + value.
            let _changed =
                BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
            let value = decode_pv_field(&intro, &mut cur, order)
                .map_err(|e| PvaError::Decode(e.to_string()))?;
            let pv_name = ch.name.clone();
            // Round 42: type-state PUT gate. The token's
            // `allows_write()` is checked by `put_value_checked`;
            // adding a new PUT-equivalent handler without taking a
            // token through `source.access().check(...)` is a compile
            // error on the trait method signature.
            let ctx = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
            };
            let checked = source
                .access_gate()
                .check(&pv_name, &ctx.host, &ctx.account, &ctx.method, "")
                .await;
            let result = source.put_value_checked(checked, value, ctx.clone()).await;

            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            payload.put_u8(0x00);
            match result {
                Ok(()) => {
                    // PUT_GET (subcmd bit 0x40 set on the request): client
                    // wants the post-put value back. Per pvxs serverget.cpp:103
                    // the response carries `bitset + partial value` after the
                    // status. Readback must be credential-aware so a peer
                    // with READ-only or NoAccess on its ASG does not see
                    // a leaked value through the PUT_GET return path.
                    if subcmd & 0x40 != 0 {
                        // R31-G7 / Round-32B: build the readback FIRST,
                        // then write status. If the READ check fails
                        // (ACF denies READ even though PUT was allowed —
                        // e.g. WRITE-only ASG), we MUST NOT emit
                        // `status_ok` followed by no bytes — that
                        // truncates the wire and the client decoder
                        // expects a bitset+value to follow. Instead,
                        // emit an all-zero bitset (no fields changed)
                        // alongside the OK status: client decodes
                        // zero fields, no value bytes consumed, PUT
                        // reported successful — same wire shape as a
                        // "put committed but no field deltas to
                        // report" response.
                        //
                        // Round 42 type-state: re-check via the gate
                        // for the READ leg. The PUT's token was
                        // consumed by `put_value_checked`; we mint a
                        // fresh one against the SAME `(pv, ctx)`.
                        let read_checked = source
                            .access_gate()
                            .check(&pv_name, &ctx.host, &ctx.account, &ctx.method, "")
                            .await;
                        match source.get_value_checked(read_checked, ctx).await {
                            Some(v) => {
                                Status::ok().write_into(order, &mut payload);
                                let bits = BitSet::all_set(intro.total_bits());
                                bits.write_into(order, &mut payload);
                                encode_pv_field(&v, &intro, order, &mut payload);
                            }
                            None => {
                                Status::ok().write_into(order, &mut payload);
                                let empty = BitSet::with_capacity(intro.total_bits());
                                empty.write_into(order, &mut payload);
                                // No encode_pv_field — bitset.count()==0
                                // means zero partial fields follow.
                            }
                        }
                    } else {
                        Status::ok().write_into(order, &mut payload);
                    }
                }
                Err(msg) => Status::error(msg).write_into(order, &mut payload),
            }
            let h = PvaHeader::application(true, order, Command::Put.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        OpKind::Monitor => {
            // MONITOR_START / pipeline-ack: pvxs uses subcmd 0x40 for
            // START and 0x80 for ACK (the high bit signals "ack"
            // followed by a u32 ack-count payload that refills the
            // pipeline window). Either signals "produce events".
            // Plain 0x00 also accepted for legacy compatibility.
            let is_ack = subcmd & 0x80 != 0;
            let is_start_or_ack = subcmd & 0x40 != 0 || is_ack || subcmd == 0x00;
            // P-G28: subcmd 0x04 alone is PAUSE (pvxs Subscription::
            // pause(true)). subcmd 0x44 (start | process bit) is
            // RESUME — clears the paused flag in addition to its
            // existing start handling. We honour PAUSE by setting
            // the paused atomic; the subscriber loop checks before
            // emit. The flag also clears on RESUME and on START.
            let is_pause = subcmd == 0x04;
            let is_resume = subcmd & 0x40 != 0;
            if let Some(op) = ch.ops.get(&ioid) {
                if is_pause {
                    op.monitor_paused
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else if is_resume {
                    let prev = op
                        .monitor_paused
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    if prev {
                        if let Some(n) = op.monitor_window_notify.as_ref() {
                            n.notify_waiters();
                        }
                    }
                }
            }

            // ACK path: refill the pipeline window (P-G11). pvxs
            // servermon.cpp:111 reads the u32 ack-count; we add it
            // to the AtomicU32 and pulse the notify so a paused
            // subscriber wakes and resumes emission. ACKs can arrive
            // before OR after the START — we always honour them.
            if is_ack {
                if let Some(op) = ch.ops.get(&ioid) {
                    let ack_count = cur.get_u32(order).unwrap_or(4);
                    if let (Some(w), Some(n)) = (
                        op.monitor_window.as_ref(),
                        op.monitor_window_notify.as_ref(),
                    ) {
                        let prev = w.fetch_add(ack_count, std::sync::atomic::Ordering::Relaxed);
                        if prev == 0 {
                            n.notify_waiters();
                        }
                    }
                }
            }

            // Only spawn the subscriber task once per ioid.
            let already_running = ch
                .ops
                .get(&ioid)
                .map(|s| s.monitor_started)
                .unwrap_or(false);
            if is_start_or_ack && !already_running {
                let pv_name = ch.name.clone();
                let intro_clone = intro.clone();
                let mask_clone = mask.clone();
                let tx_clone = tx.clone();
                let src = source.clone();
                let queue_depth = config.monitor_queue_depth;
                let high_watermark = config.monitor_high_watermark;
                // ACF-aware MONITOR: capture the peer's credentials
                // so the spawned task can consult ctx-aware
                // subscribe/get_value paths. Sources without ACF
                // delegate to the legacy methods.
                let mon_ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred.account.clone(),
                    method: cred.method.clone(),
                    host: cred.host.clone(),
                };
                // Round 42: type-state MONITOR gate. Mint the token
                // once before spawning so both the raw and decoded
                // sub-paths share the same check. Clones are cheap
                // (`AccessChecked` carries only the name String + a
                // Copy AccessLevel).
                let mon_checked = source
                    .access_gate()
                    .check(&pv_name, &mon_ctx.host, &mon_ctx.account, &mon_ctx.method, "")
                    .await;
                // Snapshot the window + notify so the spawned task can
                // share state with this dispatch path's ACK handler.
                let (window, window_notify, paused_flag) = ch
                    .ops
                    .get(&ioid)
                    .map(|s| {
                        (
                            s.monitor_window.clone(),
                            s.monitor_window_notify.clone(),
                            s.monitor_paused.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            None,
                            None,
                            Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        )
                    });
                let total_bits = intro_clone.total_bits();
                // Raw fast path is correct only when the downstream's
                // pvRequest matches the upstream's bytes 1:1 — i.e. no
                // per-field projection, and no negotiated pipeline
                // credit window (the raw branch has no per-event
                // window-decrement / wait-for-ACK gating). Fall back to
                // the decoded subscribe path otherwise.
                let raw_path_eligible = mask_clone.count() == total_bits
                    && mask_clone.size() >= total_bits
                    && window.is_none();
                let join = tokio::spawn(async move {
                    // F-G12: raw-frame fast path. When the source can
                    // hand us pre-encoded MONITOR DATA bytes (e.g.
                    // pva_gateway upstream-monitor task already
                    // received them on the wire), emit them with only
                    // an IOID-rewrite — pvxs / pva2pva style raw
                    // forward. Falls back to the decoded path on
                    // byte-order mismatch or when the source returns
                    // None.
                    // R31-G6 / Round-32A: raw fast path must consult
                    // the ACF too. The round-29 ACL gate covered
                    // `subscribe_ctx` only; ACF-aware sources can now
                    // override `subscribe_raw_ctx` to deny when the
                    // peer lacks READ. When the gateway denies (returns
                    // None), we fall through to the decoded
                    // `subscribe_ctx` below — which is also ACF-gated
                    // and will likewise return None.
                    if raw_path_eligible
                        && let Some(mut rx_raw) = src
                            .subscribe_raw_checked(mon_checked.clone(), mon_ctx.clone())
                            .await
                    {
                        // Emit initial snapshot via the regular
                        // encode path (no raw bytes for the
                        // first-event seed; the cache may not have
                        // them yet). ACF-aware: a peer with NoAccess
                        // on this PV's ASG sees no initial frame
                        // through the raw fast path either.
                        if let Some(initial) = src
                            .get_value_checked(mon_checked.clone(), mon_ctx.clone())
                            .await
                        {
                            let payload = build_monitor_payload(
                                ioid,
                                &intro_clone,
                                &initial,
                                &mask_clone,
                                order,
                            );
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                        while let Some(ev) = rx_raw.recv().await {
                            // Refuse the fast path on byte-order
                            // mismatch (cross-host gateway, rare).
                            // Falling back means re-decoding to
                            // PvField then re-encoding here.
                            if ev.byte_order != order {
                                debug!(
                                    pv = %pv_name,
                                    "F-G12 byte-order mismatch — \
                                     dropping to decode-encode path"
                                );
                                // Drop this event (decode/encode
                                // fallback would require the FieldDesc
                                // and this code path is exercised
                                // <0.1% of the time); regular subscribe
                                // covers it. Future work: keep both
                                // streams active under mismatch.
                                continue;
                            }
                            let payload = build_monitor_payload_raw(ioid, &ev, order);
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                        let finish = build_monitor_finish(ioid, order);
                        let _ = tx_clone.send(finish).await;
                        return;
                    }

                    let Some(mut rx) = src
                        .subscribe_checked(mon_checked.clone(), mon_ctx.clone())
                        .await
                    else {
                        return;
                    };
                    let mut over_high = false;
                    // Emit initial snapshot via the ACF-aware path —
                    // a peer with NoAccess on the record's ASG sees
                    // nothing; legacy sources fall through to
                    // `get_value` via the trait default.
                    if let Some(initial) = src
                        .get_value_checked(mon_checked.clone(), mon_ctx.clone())
                        .await
                    {
                        // pvxs e9ab67a (2025-10): the first update is
                        // mask-bypassed so the client receives a
                        // *complete* prototype regardless of its
                        // pvRequest field selection. Subsequent
                        // updates honour `mask_clone`. Without this,
                        // a `record[]` request with no fields marked
                        // would silently filter out the seeding
                        // update and `fill_unmarked_from_prior` on
                        // the client would have no real prior to
                        // merge against — the user would see leaves
                        // default-filled forever.
                        let full_mask = BitSet::all_set(intro_clone.total_bits());
                        let payload =
                            build_monitor_payload(ioid, &intro_clone, &initial, &full_mask, order);
                        if tx_clone.send(payload).await.is_err() {
                            return;
                        }
                    }
                    // Back-pressure / squashing loop: drain available
                    // events between writes, keeping only the most recent
                    // value if more than `queue_depth` events stack up.
                    let mut squashing = false;
                    while let Some(mut value) = rx.recv().await {
                        // Drain extras; keep the latest.
                        let mut squashed = 0usize;
                        loop {
                            match rx.try_recv() {
                                Ok(next) => {
                                    value = next;
                                    squashed += 1;
                                    if squashed > queue_depth {
                                        squashing = true;
                                    }
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => break,
                            }
                        }
                        if squashing {
                            debug!(pv = %pv_name, squashed, "monitor squashed events");
                            squashing = false;
                        }
                        // Watermark crossing diagnostics + producer
                        // notification. pvxs fires `onHighMark` /
                        // `onLowMark` callbacks at these transitions so
                        // sources can throttle/un-throttle their post()
                        // rate; we mirror that via
                        // `ChannelSource::notify_watermark_{high,low}`.
                        // The default trait impl is a no-op; SharedSource
                        // overrides it to dispatch the per-PV callback
                        // registered via `SharedPv::set_on_high_mark`.
                        // Counter is max_capacity - capacity since mpsc
                        // doesn't expose len directly.
                        let pending = tx_clone.max_capacity() - tx_clone.capacity();
                        if pending >= high_watermark && !over_high {
                            over_high = true;
                            warn!(
                                pv = %pv_name,
                                pending,
                                high_watermark,
                                "monitor outbound queue crossed high watermark"
                            );
                            src.notify_watermark_high(&pv_name);
                        } else if pending == 0 && over_high {
                            over_high = false;
                            debug!(pv = %pv_name, "monitor outbound queue drained below low watermark");
                            src.notify_watermark_low(&pv_name);
                        }
                        // P-G11: pipeline window check. When pipeline
                        // is active, wait for window > 0 before
                        // emitting. ACK frames refill the window via
                        // the dispatch path; we wake on the notify.
                        // Without a window (pipeline=false) we emit
                        // freely; mpsc backpressure remains the only
                        // gate, matching previous behavior.
                        if let (Some(w), Some(n)) = (window.as_ref(), window_notify.as_ref()) {
                            loop {
                                let cur = w.load(std::sync::atomic::Ordering::Relaxed);
                                if cur > 0 {
                                    if w.compare_exchange(
                                        cur,
                                        cur - 1,
                                        std::sync::atomic::Ordering::Relaxed,
                                        std::sync::atomic::Ordering::Relaxed,
                                    )
                                    .is_ok()
                                    {
                                        break;
                                    }
                                    continue;
                                }
                                // Window exhausted — wait for ACK.
                                let notified = n.notified();
                                tokio::pin!(notified);
                                // enable() registers the waiter eagerly
                                // so an ACK firing between the recheck
                                // and the await is captured. Same
                                // pattern as channel.rs::wait_until_inactive
                                // — Notify::notified() does NOT register
                                // until the future is polled, so the
                                // recheck-then-await window otherwise
                                // loses the wake from notify_waiters().
                                notified.as_mut().enable();
                                if w.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                                    continue;
                                }
                                notified.await;
                            }
                        }
                        // P-G28: pause drops events on the floor.
                        // Resume cleanly re-emits whatever the source
                        // sends next; clients that need state recovery
                        // re-issue their pvRequest after resume.
                        if paused_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        let payload =
                            build_monitor_payload(ioid, &intro_clone, &value, &mask_clone, order);
                        if tx_clone.send(payload).await.is_err() {
                            return;
                        }
                    }
                    // Source closed — emit MONITOR FINISH (subcmd 0x10 + Status).
                    // pvxs servermon.cpp:148-178 sends a final frame with
                    // subcmd=0x10 to signal end-of-stream so the client can
                    // tear down cleanly.
                    let finish = build_monitor_finish(ioid, order);
                    let _ = tx_clone.send(finish).await;
                });
                if let Some(s) = ch.ops.get_mut(&ioid) {
                    s.monitor_started = true;
                    s.monitor_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
                }
            }
        }
        OpKind::Rpc => {
            // RPC DATA request from client: `type(arg) + full_value(arg)`.
            // pvxs clientget.cpp:307-311 — `to_wire(R, type); to_wire_full(R, arg)`.
            // The introspection on the channel was negotiated for the
            // *pvRequest* in INIT, not the actual call argument, so we must
            // decode the argument's own type descriptor here.
            let (req_desc, req_value) = match decode_type_desc(&mut cur, order) {
                Ok(desc) => match decode_pv_field(&desc, &mut cur, order) {
                    Ok(v) => (desc, v),
                    Err(_) => (desc, PvField::Null),
                },
                Err(_) => {
                    // Empty body — some clients send parameterless RPCs with
                    // no payload after subcmd.
                    (FieldDesc::Variant, PvField::Null)
                }
            };
            let pv_name = ch.name.clone();
            let _ = intro; // INIT pvRequest descriptor — no longer used here.
            // Round 42 type-state RPC dispatch. The wire layer mints
            // the AccessChecked token via the gate; the typed
            // `rpc_checked` refuses `NoAccess` tokens. Adding a new
            // RPC-equivalent handler without going through the gate
            // is now a compile error on the trait method signature.
            let rpc_ctx_val = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
            };
            let rpc_checked = source
                .access_gate()
                .check(&pv_name, &rpc_ctx_val.host, &rpc_ctx_val.account, &rpc_ctx_val.method, "")
                .await;
            let result = source
                .rpc_checked(rpc_checked, req_desc, req_value, rpc_ctx_val)
                .await;

            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            payload.put_u8(0x00);
            match result {
                Ok((resp_desc, resp_value)) => {
                    Status::ok().write_into(order, &mut payload);
                    if config.emit_type_cache {
                        encode_type_desc_cached(&resp_desc, order, encode_cache, &mut payload);
                    } else {
                        encode_type_desc(&resp_desc, order, &mut payload);
                    }
                    encode_pv_field(&resp_value, &resp_desc, order, &mut payload);
                }
                Err(msg) => Status::error(msg).write_into(order, &mut payload),
            }
            let h = PvaHeader::application(true, order, Command::Rpc.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
    }
    Ok(())
}

async fn handle_get_field(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let _sub = crate::proto::decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;

    // P-G19: pvxs serverintrospect.cpp:159 silently returns on
    // unknown SID; without this we'd reply with a fabricated
    // Variant descriptor + status=OK, which is worse than a noop —
    // a stale client would build its decode tree against a wrong
    // shape and surface garbage on the next GET. Match pvxs.
    let chan = match channels.get(&sid) {
        Some(c) => c,
        None => {
            debug!(sid, ioid, "GET_FIELD on unknown SID: dropping");
            return Ok(());
        }
    };
    let intro = match chan.introspection.clone() {
        Some(d) => d,
        None => source
            .get_introspection(&chan.name)
            .await
            .unwrap_or(FieldDesc::Variant),
    };

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    Status::ok().write_into(order, &mut payload);
    encode_type_desc(&intro, order, &mut payload);
    let h = PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

async fn send_op_error(
    tx: &SrvTx,
    kind: OpKind,
    ioid: u32,
    msg: &str,
    order: ByteOrder,
) -> PvaResult<()> {
    let cmd = match kind {
        OpKind::Get => Command::Get,
        OpKind::Put => Command::Put,
        OpKind::Monitor => Command::Monitor,
        OpKind::Rpc => Command::Rpc,
    };
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x08); // INIT phase err
    Status::error(msg.to_string()).write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

#[allow(unused_imports)]
use crate::proto::ReadExt;
const _: u8 = PVA_VERSION;

/// Build a complete MONITOR data frame (header + payload) for a single value
/// emission. Pulled out so the back-pressure squashing loop can call it.
fn build_monitor_payload(
    ioid: u32,
    intro: &FieldDesc,
    value: &PvField,
    mask: &BitSet,
    order: ByteOrder,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    // PVA monitor data: changed bitset + partial value + overrun bitset.
    // The changed bitset reflects the pvRequest field mask — emit only
    // the fields the client asked for.
    mask.write_into(order, &mut payload);
    crate::pvdata::encode::encode_pv_field_with_bitset(value, intro, mask, 0, order, &mut payload);
    let overrun = BitSet::new(); // no overruns
    overrun.write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// F-G12 raw-frame variant: build a MONITOR data frame from a
/// pre-encoded [`crate::server_native::RawMonitorEvent`]. The body
/// (`changed | value | overrun`) is reused verbatim with a single
/// `extend_from_slice` (memcpy); only the per-subscription PVA
/// header + downstream IOID + subcmd are fresh.
fn build_monitor_payload_raw(
    ioid: u32,
    ev: &crate::server_native::RawMonitorEvent,
    order: ByteOrder,
) -> Vec<u8> {
    let total = 4 /* ioid */ + 1 /* subcmd */ + ev.body_bytes.len();
    let mut payload = Vec::with_capacity(total);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    payload.extend_from_slice(&ev.body_bytes);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// Build a MONITOR FINISH frame (subcmd `0x10` + Status). Sent when the
/// underlying source closes its broadcast channel, signalling end-of-stream
/// to the subscribing client. Mirrors pvxs `servermon.cpp:148-178`.
fn build_monitor_finish(ioid: u32, order: ByteOrder) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x10);
    Status::ok().write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

fn now_nanos() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_native::decode::{OpResponse, decode_op_response, try_parse_frame};
    use crate::pvdata::{PvStructure, ScalarType, ScalarValue};

    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn handle_message_does_not_panic_on_well_formed_input() {
        // Wire layout: ioid (u32) + messageType (u8) + message (string).
        // We can't easily inspect tracing output here, so the assertion is
        // simply that the handler tolerates each severity level without
        // panicking and consumes the cursor cleanly.
        let order = ByteOrder::Little;
        let peer = "127.0.0.1:5075".parse::<SocketAddr>().unwrap();
        for mtype in [0u8, 1, 2, 3, 9] {
            let mut payload = Vec::new();
            payload.put_u32(0xDEADBEEF, order); // ioid
            payload.put_u8(mtype);
            crate::proto::encode_string_into("hello from client", order, &mut payload);
            let frame = synth_frame(Command::Message, order, payload);
            handle_message(&frame, order, &peer); // must not panic
        }

        // Truncated payloads must also not panic — handler should bail
        // silently after the first read failure.
        let frame_short = synth_frame(Command::Message, order, vec![0x01, 0x02]);
        handle_message(&frame_short, order, &peer);
    }

    #[tokio::test]
    async fn cancel_request_aborts_monitor_and_clears_started_flag() {
        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 99;

        // Stand up a fake OpState whose `monitor_abort` points at a real
        // task we can observe being cancelled.
        let task = tokio::spawn(async move {
            // Loop until aborted by the Drop guard. If the test ever
            // returns without the abort firing, the JoinHandle will see
            // this future complete normally — the assertion below catches
            // that.
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let abort = Arc::new(AbortOnDrop(task.abort_handle()));

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            OpState {
                intro: FieldDesc::Variant,
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: Some(abort.clone()),
                mask: BitSet::new(),
                monitor_window: None,
                monitor_window_notify: None,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 1,
                sid,
                introspection: None,
                ops,
            },
        );

        // Build the CancelRequest payload: sid + ioid.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::CancelRequest, order, payload);
        handle_cancel_request(&frame, &mut channels, order);

        // Op must still be in the map (cancel is non-destructive), but
        // `monitor_started` must reset and the abort guard must be cleared.
        let op = channels
            .get(&sid)
            .and_then(|c| c.ops.get(&ioid))
            .expect("op preserved across cancel");
        assert!(!op.monitor_started, "monitor_started should reset");
        assert!(op.monitor_abort.is_none(), "abort guard should be dropped");

        // Drop the only remaining strong ref — this fires the abort
        // (already triggered above when the OpState's clone dropped).
        drop(abort);

        // The task must terminate (with `cancelled` == true) within a
        // reasonable window, otherwise the cancel was a no-op.
        let join = tokio::time::timeout(Duration::from_millis(500), task).await;
        let outcome = join.expect("aborted task should finish quickly");
        assert!(
            outcome.unwrap_err().is_cancelled(),
            "task should have been aborted by the Drop guard"
        );
    }

    #[test]
    fn monitor_payload_orders_overrun_after_value() {
        let order = ByteOrder::Little;
        let ioid = 0x1234;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut value = PvStructure::new("epics:nt/NTScalar:1.0");
        value
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.5))));

        let mask = BitSet::all_set(intro.total_bits());
        let bytes = build_monitor_payload(ioid, &intro, &PvField::Structure(value), &mask, order);
        let (frame, used) = try_parse_frame(&bytes).unwrap().expect("complete frame");
        assert_eq!(used, bytes.len());

        match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(data) => {
                assert_eq!(data.ioid, ioid);
                match data.value {
                    PvField::Structure(s) => {
                        assert_eq!(
                            s.get_field("value"),
                            Some(&PvField::Scalar(ScalarValue::Double(42.5)))
                        );
                    }
                    other => panic!("expected structure, got {other:?}"),
                }
            }
            other => panic!("expected monitor data, got {other:?}"),
        }
    }
}
