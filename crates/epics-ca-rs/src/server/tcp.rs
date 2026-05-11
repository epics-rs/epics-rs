use epics_base_rs::runtime::sync::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Maximum accumulated TCP read buffer per client (DoS guard).
/// Mirrors the client-side cap in `client/transport.rs`.
const MAX_ACCUMULATED: usize = 1024 * 1024; // 1 MB

/// Optional application-level idle timeout before forcibly closing a TCP
/// client. Disabled by default — OS-level TCP keepalive (set in `accept_loop`,
/// 15s idle + 5s probes) is the primary half-open detector and matches C
/// epics-base rsrv (`caservertask.c:1456` sets only `SO_KEEPALIVE`, with no
/// application-level idle timeout).
///
/// A C client receiving a continuous monitor stream may never send
/// `CA_PROTO_ECHO` (libca resets its echo timer on every received frame from
/// the server), so an inactivity timeout based purely on incoming reads
/// produces false-positive disconnects on healthy connections — the bug
/// archaeology REVIEW for this is in `archaeology/REVIEWS/`. Operators who
/// want a defensive cap (e.g., NAT environments where TCP keepalive is
/// unreliable) can set `EPICS_CAS_INACTIVITY_TMO` to a positive value;
/// values < 30 are clamped to 30 to avoid pathological short timeouts.
fn inactivity_timeout() -> Option<Duration> {
    epics_base_rs::runtime::env::get("EPICS_CAS_INACTIVITY_TMO")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .map(|v| Duration::from_secs_f64(v.max(30.0)))
}

/// Read into `buf` with an optional idle cap. If `cap` is `None`, the read
/// is unbounded (matches C `recv()` blocking semantics in `camsgtask.c`);
/// if `cap` is `Some(d)`, returns `Err(d)` after `d` of inactivity.
async fn read_with_optional_timeout<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    cap: Option<Duration>,
) -> Result<std::io::Result<usize>, Duration> {
    match cap {
        None => Ok(reader.read(buf).await),
        Some(d) => match tokio::time::timeout(d, reader.read(buf)).await {
            Ok(r) => Ok(r),
            Err(_) => Err(d),
        },
    }
}

/// Maximum simultaneous channels per CA client (EPICS_CAS_MAX_CHANNELS).
fn max_channels_per_client() -> usize {
    epics_base_rs::runtime::env::get("EPICS_CAS_MAX_CHANNELS")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4096)
        .max(1)
}

/// Maximum subscriptions per channel (EPICS_CAS_MAX_SUBS_PER_CHAN).
fn max_subs_per_channel() -> usize {
    epics_base_rs::runtime::env::get("EPICS_CAS_MAX_SUBS_PER_CHAN")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .max(1)
}

/// Forward-DNS verification for `EPICS_CAS_USE_HOST_NAMES=YES`.
///
/// Resolve `claimed` (the client-supplied hostname) to a list of IPs
/// and require `peer` (the actual TCP peer IP) to appear among them.
/// Returns `true` only when a match is found, `false` on resolution
/// failure or mismatch — fail closed.
///
/// Done via `tokio::net::lookup_host` which dispatches to the
/// platform resolver (getaddrinfo), so honours `/etc/hosts`, NIS,
/// LDAP, etc. The DNS lookup is per-HOST_NAME-message so the cost
/// is paid once per CA client connection, not per put / per
/// channel.
async fn host_resolves_to_peer(claimed: &str, peer: std::net::IpAddr) -> bool {
    if claimed.is_empty() {
        return false;
    }
    // `lookup_host` requires a port — a sentinel `:0` is fine since
    // we discard everything except the IP.
    let target = format!("{claimed}:0");
    match tokio::net::lookup_host(target).await {
        Ok(mut iter) => iter.any(|sa| sa.ip() == peer),
        Err(_) => false,
    }
}

/// Per-socket send timeout. Without this, a client that stops
/// reading (frozen GUI, dead viewer holding the socket open) causes
/// every server `write` to block once the kernel send buffer fills,
/// stalling the whole per-client dispatcher task. C rsrv defaults
/// SO_SNDTIMEO to 5 s; we honour the same default and let
/// `EPICS_CAS_SEND_TMO` override.
fn send_timeout() -> Duration {
    epics_base_rs::runtime::env::get("EPICS_CAS_SEND_TMO")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| Duration::from_secs_f64(v.max(0.1)))
        .unwrap_or(Duration::from_secs(5))
}

/// Cap on `TlsAcceptor::accept` duration. Round 8 C-G12: without this
/// a peer that completes TCP but stalls during ClientHello holds a
/// connection slot until OS-level keepalive (15s/5s probes) reaps it
/// (~30s); coordinated peers can exhaust the listener under
/// `EPICS_CAS_MAX_CONNECTIONS`. Default 10 s, override via
/// `EPICS_CAS_TLS_HANDSHAKE_TMO`. Floored at 1s.
#[cfg(feature = "experimental-rust-tls")]
fn tls_handshake_timeout() -> Duration {
    epics_base_rs::runtime::env::get("EPICS_CAS_TLS_HANDSHAKE_TMO")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| Duration::from_secs_f64(v.max(1.0)))
        .unwrap_or(Duration::from_secs(10))
}

/// Connection lifecycle event broadcast by the TCP listener.
///
/// Marked `#[non_exhaustive]` so subsequent variants (e.g. per-monitor
/// events) can be added without breaking downstream `match` arms.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ServerConnectionEvent {
    /// New client connection accepted.
    Connected(SocketAddr),
    /// Client connection closed.
    Disconnected(SocketAddr),
    /// `CA_PROTO_CREATE_CHAN` succeeded for `pv_name` on `peer`. The
    /// `cid` is the client-supplied channel id from the request — pass
    /// it through to consumers so multiple channels for the same
    /// `(peer, pv_name)` pair don't collapse into one refcount slot.
    /// Used by the CA gateway to drive per-PV `Inactive` → `Active`
    /// transitions (see `ca_gateway::cache::GwPvEntry::add_subscriber`).
    ChannelCreated {
        peer: SocketAddr,
        pv_name: String,
        cid: u32,
    },
    /// `CA_PROTO_CLEAR_CHANNEL` (or implicit teardown) closed a channel
    /// for `pv_name` on `peer`. The `cid` matches the corresponding
    /// [`Self::ChannelCreated`] event one-to-one. Reverse of that event.
    ChannelCleared {
        peer: SocketAddr,
        pv_name: String,
        cid: u32,
    },
}

use crate::protocol::*;
use crate::server::monitor::{FlowControlGate, spawn_monitor_sender};
use epics_base_rs::error::CaResult;
use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig};
use epics_base_rs::server::database::{PvDatabase, PvEntry, parse_pv_name};
use epics_base_rs::server::pv::ProcessVariable;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::{DbFieldType, EpicsValue, encode_dbr, native_type_for_dbr};

#[derive(Clone)]
enum ChannelTarget {
    SimplePv(Arc<ProcessVariable>),
    RecordField {
        record: Arc<RwLock<RecordInstance>>,
        field: String,
    },
}

struct ChannelEntry {
    target: ChannelTarget,
    cid: u32,
    /// PV name as the client originally requested it (with any
    /// `.FIELD` suffix). Retained so the `ChannelCleared` lifecycle
    /// event can emit the same name as `ChannelCreated`.
    pv_name: String,
}

struct SubscriptionEntry {
    target: ChannelTarget,
    channel_sid: u32,
    sub_id: u32,
    data_type: u16,
    task: tokio::task::JoinHandle<()>,
}

struct ClientState {
    channels: HashMap<u32, ChannelEntry>,
    subscriptions: HashMap<u32, SubscriptionEntry>,
    channel_access: HashMap<u32, AccessLevel>,
    next_sid: AtomicU32,
    /// Recycled SIDs from channels destroyed via CLEAR_CHANNEL. C-G9:
    /// without recycling, `next_sid` would wrap after 2³² channel
    /// creations and start handing out SIDs that collide with live
    /// channels. epics-base `rsrv/camessage.c` uses
    /// `freeListItemPvt` for the same reason. We use a Vec stack
    /// (LIFO) so the most-recently-freed SID is reused first —
    /// keeps the active set's SIDs clustered near the low end.
    free_sids: Vec<u32>,
    hostname: String,
    username: String,
    acf: Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
    tcp_port: u16,
    client_minor_version: u16,
    flow_control: Arc<FlowControlGate>,
    /// One-shot flag — set when channels.len() crosses 90% of the
    /// per-client cap. Prevents log spam on every subsequent
    /// CREATE_CHAN once the warning has fired.
    channel_limit_warned: bool,
    /// Peer address as a string, retained for audit events.
    peer: String,
    /// Optional audit logger. When None the audit hot path is a single
    /// branch test and no allocation.
    audit: Option<crate::audit::AuditLogger>,
    /// Optional per-client token bucket. None disables rate limiting.
    rate_limiter: Option<crate::server::rate_limit::RateLimiter>,
    /// Consecutive denied messages — disconnect when this exceeds the
    /// configured strike threshold.
    rate_limit_strikes: u32,
    rate_limit_strike_threshold: u32,
    /// Capability-token verifier shared across all clients on this
    /// listener. When set, CLIENT_NAME payloads beginning with `cap:`
    /// are verified before the resolved subject is used as the ACF
    /// username.
    #[cfg(feature = "cap-tokens")]
    cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
    /// Pending WRITE_NOTIFY completion tasks. Each entry is the
    /// AbortHandle of a task awaiting `put_notify_tx` for an async
    /// record write. Aborted on connection drop so a stuck async
    /// device doesn't leak the task forever.
    write_notify_tasks: Vec<tokio::task::AbortHandle>,
}

impl ClientState {
    fn new(acf: Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>, tcp_port: u16) -> Self {
        Self {
            channels: HashMap::new(),
            subscriptions: HashMap::new(),
            channel_access: HashMap::new(),
            next_sid: AtomicU32::new(1),
            free_sids: Vec::new(),
            hostname: String::new(),
            username: String::new(),
            acf,
            tcp_port,
            client_minor_version: 0,
            flow_control: Arc::new(FlowControlGate::default()),
            channel_limit_warned: false,
            peer: String::new(),
            audit: None,
            rate_limiter: None,
            rate_limit_strikes: 0,
            rate_limit_strike_threshold: 0,
            #[cfg(feature = "cap-tokens")]
            cap_token_verifier: None,
            write_notify_tasks: Vec::new(),
        }
    }

    async fn audit(&self, event: &str, pv: &str, value: &str, result: &str) {
        if let Some(ref logger) = self.audit {
            logger
                .log(crate::audit::AuditEvent {
                    event,
                    peer: &self.peer,
                    user: &self.username,
                    host: &self.hostname,
                    pv,
                    value,
                    result,
                })
                .await;
        }
    }

    fn alloc_sid(&mut self) -> u32 {
        // C-G9: prefer recycled SIDs from CLEAR_CHANNEL'd channels.
        // Falls back to monotonic counter only when the free list is
        // empty, which prevents wraparound collisions on long-uptime
        // high-churn servers (epics-base rsrv `freeListItemPvt`
        // parity).
        if let Some(sid) = self.free_sids.pop() {
            return sid;
        }
        self.next_sid.fetch_add(1, Ordering::Relaxed)
    }

    /// Return a SID to the free list when its channel is destroyed.
    fn release_sid(&mut self, sid: u32) {
        self.free_sids.push(sid);
    }

    /// Round 44: return the type-state-wrapped access token for a
    /// SID. Op handlers MUST consult this — direct reads of the
    /// underlying `channel_access` HashMap bypass the typed gate
    /// and recreate the missed-path defects fixed in rounds 38-39.
    /// Missing SIDs map to a "denied" token so a corrupted
    /// channel-table state can never silently grant access.
    fn lookup_access(&self, sid: u32) -> crate::server::access_token::CaAccessChecked {
        use crate::server::access_token::CaAccessChecked;
        match self.channel_access.get(&sid).copied() {
            Some(level) => CaAccessChecked::from_level(level),
            None => CaAccessChecked::denied(),
        }
    }

    /// Compute access rights bits for a channel target.
    async fn compute_access(&self, target: &ChannelTarget) -> u32 {
        match target {
            ChannelTarget::SimplePv(_) => {
                let guard = self.acf.read().await;
                if let Some(ref acf_cfg) = *guard {
                    // Simple PVs have no per-record ASL field; treat
                    // them as ASL=0 so the most-restrictive rule
                    // applies. Matches the C IOC's behaviour for
                    // names that never went through `dbAddMember`.
                    match acf_cfg.check_access_asl("DEFAULT", &self.hostname, &self.username, 0) {
                        AccessLevel::ReadWrite => 3,
                        AccessLevel::Read => 1,
                        AccessLevel::NoAccess => 0,
                    }
                } else {
                    3
                }
            }
            ChannelTarget::RecordField { record, field: f } => {
                let instance = record.read().await;
                let is_ro = instance
                    .record
                    .field_list()
                    .iter()
                    .find(|fd| fd.name == f.as_str())
                    .map(|fd| fd.read_only)
                    .unwrap_or(false);
                // R48-G2 (Round 48): read-only field-ness must AND
                // with ACF, never replace it. Pre-fix the read-only
                // branch returned `Read`(1) unconditionally — a
                // peer whose ACF resolved to `NoAccess` could still
                // READ / EVENT_ADD on every read-only field because
                // the cached access_rights skipped the ACF check
                // entirely. Now ACF runs first; the read-only flag
                // only strips the WRITE bit from the result.
                let guard = self.acf.read().await;
                let acf_level = if let Some(ref acf_cfg) = *guard {
                    // Round-33A (R33-G4): thread the per-record
                    // ASL into the ACF check so `RULE(N, …)`
                    // gates correctly disable rules whose level
                    // is below the record's ASL.
                    let asg = &instance.common.asg;
                    let asl = instance.common.asl;
                    acf_cfg.check_access_asl(asg, &self.hostname, &self.username, asl)
                } else {
                    AccessLevel::ReadWrite
                };
                match (acf_level, is_ro) {
                    (AccessLevel::NoAccess, _) => 0,
                    (AccessLevel::Read, _) => 1,
                    (AccessLevel::ReadWrite, true) => 1,
                    (AccessLevel::ReadWrite, false) => 3,
                }
            }
        }
    }
}

/// Run the TCP listener for CA connections.
/// Tries to bind to the configured port first; falls back to an ephemeral port
/// (port 0) if the configured port is already in use.
///
/// Notifies `beacon_reset` on each client connect/disconnect so the beacon
/// emitter restarts its fast beacon cycle (matching C EPICS behavior).
#[allow(clippy::too_many_arguments)]
pub async fn run_tcp_listener(
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
    acf_reload_tx: broadcast::Sender<()>,
    tcp_port_tx: tokio::sync::oneshot::Sender<u16>,
    beacon_reset: std::sync::Arc<tokio::sync::Notify>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    audit: Option<crate::audit::AuditLogger>,
    drain: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<
        Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>,
    >,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()> {
    // C-G11: honor EPICS_CAS_INTF_ADDR_LIST for the TCP listener
    // (was previously ignored — only the UDP responder respected
    // it). Bind to the first configured interface; if the list is
    // empty (the common case) fall back to 0.0.0.0. Multi-interface
    // multi-listener support is left for a future round — operators
    // who need it currently bind 0.0.0.0 and apply firewall rules.
    let bind_ip: std::net::IpAddr = {
        let cfg = super::addr_list::from_env();
        cfg.intf_addrs
            .first()
            .map(|a| std::net::IpAddr::V4(*a))
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    };
    let listener = match TcpListener::bind((bind_ip, port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            TcpListener::bind((bind_ip, 0)).await?
        }
        Err(e) => return Err(e.into()),
    };
    let actual_port = listener.local_addr()?.port();
    let _ = tcp_port_tx.send(actual_port);

    // D-G1: track per-connection tasks in a JoinSet so they're
    // aborted as a unit when this accept-loop future is dropped (e.g.
    // CaServer shutdown via tcp_abort.abort()). Without this, every
    // per-conn task ran detached and lingered until its internal
    // idle/op timeout. The select! arm on `conn_tasks.join_next()`
    // also reaps completed tasks so the set doesn't accumulate
    // finished JoinHandles.
    let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        // Drain mode: stop accepting new connections. Existing
        // connections continue to be served by their own tasks; the
        // CaServer::run() loop coordinates the grace period and the
        // ultimate exit.
        if drain.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!("TCP listener: drain mode set, exiting accept loop");
            return Ok(());
        }
        let (stream, peer) = tokio::select! {
            biased;
            res = listener.accept() => res?,
            // Drain finished connection tasks. Returns None when the
            // set is empty — that branch resolves immediately, but
            // `biased` makes the listener arm preferred so we never
            // starve incoming accepts.
            Some(_) = conn_tasks.join_next() => continue,
        };
        if drain.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(peer = %peer, "drain mode: rejecting new connection");
            drop(stream);
            continue;
        }
        tracing::info!(peer = %peer, "CA client connected");
        metrics::counter!("ca_server_accepts_total").increment(1);
        metrics::gauge!("ca_server_clients_active").increment(1.0);
        let db = db.clone();
        let acf = acf.clone();
        let beacon_reset = beacon_reset.clone();
        beacon_reset.notify_one();
        if let Some(tx) = &conn_events {
            let _ = tx.send(ServerConnectionEvent::Connected(peer));
        }
        let conn_events = conn_events.clone();
        let acf_reload_rx = acf_reload_tx.subscribe();
        let audit = audit.clone();
        // Read the latest server config under the RwLock so a
        // concurrent reload_tls() takes effect for the *next* accept
        // without restarting the listener. Cheap read lock — only
        // contended against rare reload write locks.
        #[cfg(feature = "experimental-rust-tls")]
        let tls_acceptor = tls.as_ref().and_then(|slot| {
            slot.read()
                .ok()
                .map(|guard| tokio_rustls::TlsAcceptor::from(guard.clone()))
        });

        // Enable OS-level TCP keepalive on accepted socket so half-open
        // connections (e.g. NAT timeout, gateway down) are detected within
        // ~30s. Mirrors client-side keepalive in client/transport.rs.
        {
            let sock = socket2::SockRef::from(&stream);
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(Duration::from_secs(15))
                .with_interval(Duration::from_secs(5));
            let _ = sock.set_keepalive(true);
            let _ = sock.set_tcp_keepalive(&keepalive);
            // SO_SNDTIMEO is set as a defence-in-depth (matches C
            // rsrv default 5s, configurable via EPICS_CAS_SEND_TMO),
            // but on a non-blocking tokio socket the kernel does NOT
            // apply it — a stuck client where the kernel send buffer
            // fills would still leave `poll_write` Pending forever.
            // The actual stall guard is the `tokio::time::timeout`
            // wrapping `dispatch_message` in `handle_client`'s read
            // loop (search for "send_timeout()" below).
            let _ = sock.set_write_timeout(Some(send_timeout()));
        }
        let _ = stream.set_nodelay(true);

        #[cfg(feature = "cap-tokens")]
        let cap_token_verifier_for_client = cap_token_verifier.clone();
        conn_tasks.spawn(async move {
            // TLS dispatch: when configured, wrap the accepted TCP
            // stream in a TlsAcceptor handshake. The client cert (if
            // any) is harvested afterwards for mTLS identity.
            let result: CaResult<()> = {
                #[cfg(feature = "experimental-rust-tls")]
                {
                    if let Some(acceptor) = tls_acceptor {
                        // C-G12: cap the TLS handshake. A peer that
                        // completes TCP but stalls during ClientHello
                        // would otherwise hold a connection slot until
                        // OS keepalive reaps it (~30s).
                        let hs =
                            tokio::time::timeout(tls_handshake_timeout(), acceptor.accept(stream))
                                .await;
                        match hs {
                            Err(_) => {
                                tracing::warn!(peer = %peer,
                                    timeout = ?tls_handshake_timeout(),
                                    "TLS handshake timed out");
                                Err(epics_base_rs::error::CaError::Protocol(
                                    "TLS handshake timeout".into(),
                                ))
                            }
                            Ok(Ok(tls_stream)) => {
                                // Extract verified peer identity from the
                                // client certificate, if presented.
                                let identity = tls_stream
                                    .get_ref()
                                    .1
                                    .peer_certificates()
                                    .and_then(|chain| chain.first())
                                    .map(crate::tls::identity_from_cert);
                                if let Some(ref id) = identity {
                                    tracing::info!(peer = %peer, identity = %id,
                                        "mTLS identity verified");
                                }
                                handle_client(
                                    tls_stream,
                                    peer,
                                    db,
                                    acf,
                                    acf_reload_rx,
                                    actual_port,
                                    identity,
                                    audit,
                                    conn_events.clone(),
                                    #[cfg(feature = "cap-tokens")]
                                    cap_token_verifier_for_client.clone(),
                                )
                                .await
                            }
                            Ok(Err(e)) => {
                                tracing::warn!(peer = %peer, error = %e,
                                    "TLS handshake failed");
                                Err(epics_base_rs::error::CaError::Io(e))
                            }
                        }
                    } else {
                        handle_client(
                            stream,
                            peer,
                            db,
                            acf,
                            acf_reload_rx,
                            actual_port,
                            None,
                            audit,
                            conn_events.clone(),
                            #[cfg(feature = "cap-tokens")]
                            cap_token_verifier_for_client.clone(),
                        )
                        .await
                    }
                }
                #[cfg(not(feature = "experimental-rust-tls"))]
                {
                    handle_client(
                        stream,
                        peer,
                        db,
                        acf,
                        acf_reload_rx,
                        actual_port,
                        None,
                        audit,
                        conn_events.clone(),
                        #[cfg(feature = "cap-tokens")]
                        cap_token_verifier_for_client.clone(),
                    )
                    .await
                }
            };
            beacon_reset.notify_one();
            if let Some(tx) = &conn_events {
                let _ = tx.send(ServerConnectionEvent::Disconnected(peer));
            }
            metrics::gauge!("ca_server_clients_active").decrement(1.0);
            metrics::counter!("ca_server_disconnects_total").increment(1);
            if let Err(e) = result {
                // Suppress normal disconnection errors (client closed connection)
                let is_disconnect = matches!(
                    e,
                    epics_base_rs::error::CaError::Io(ref io) if matches!(
                        io.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    )
                );
                if is_disconnect {
                    tracing::debug!(peer = %peer, "client disconnected");
                } else {
                    tracing::warn!(peer = %peer, error = %e, "client handler error");
                }
            } else {
                tracing::debug!(peer = %peer, "client disconnected cleanly");
            }
        });
    }
}

/// Handle one CA client over the supplied stream.
///
/// `initial_hostname` is the verified peer identity from the TLS
/// handshake (mTLS only). When `Some`, it takes precedence over
/// `peer.ip()` for the `state.hostname` ACF key — the
/// cryptographically authenticated identity is always more
/// trustworthy than the network address.
#[allow(clippy::too_many_arguments)]
async fn handle_client<S>(
    stream: S,
    peer: SocketAddr,
    db: Arc<PvDatabase>,
    acf: Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
    mut acf_reload_rx: broadcast::Receiver<()>,
    tcp_port: u16,
    initial_hostname: Option<String>,
    audit: Option<crate::audit::AuditLogger>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    // Bigger BufWriter so a 100-PV batched response burst (~3 KB) fits
    // without auto-flushing mid-batch. The dispatch hot-path no longer
    // calls `flush()` per message — `handle_client` flushes once per
    // outer read iteration after the inner message-drain loop, which
    // turns N small TCP writes into one. Default 8 KB was hit at ~330
    // responses; 64 KB covers the common bulk_caget(100) case with
    // headroom for follow-on monitor events queued in the same tick.
    let writer = Arc::new(Mutex::new(BufWriter::with_capacity(64 * 1024, writer)));
    let mut state = ClientState::new(acf, tcp_port);
    #[cfg(feature = "cap-tokens")]
    {
        state.cap_token_verifier = cap_token_verifier;
    }
    // Default hostname: verified TLS identity if present, otherwise the
    // peer IP. Matches C rsrv default with EPICS_CAS_USE_HOST_NAMES=NO,
    // upgraded transparently when mTLS is in effect.
    state.hostname = initial_hostname.unwrap_or_else(|| peer.ip().to_string());
    state.peer = peer.to_string();
    state.audit = audit;
    let rl_cfg = crate::server::rate_limit::RateLimitConfig::from_env();
    state.rate_limiter = rl_cfg.build();
    state.rate_limit_strike_threshold = rl_cfg.strike_threshold;
    state.audit("connect", "", "", "ok").await;
    let mut reader = reader;

    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();
    let inactivity = inactivity_timeout();

    loop {
        // Bound read with inactivity timeout so a fully-silent half-open
        // connection eventually gets cleaned up even if OS keepalive failed.
        // Race the read against ACF reload notifications so a `reload_acf*()`
        // call promptly re-pushes CA_PROTO_ACCESS_RIGHTS for every open
        // channel — RSRV's `sendAllUpdateAS` analog.
        let n = tokio::select! {
            biased;
            reload = acf_reload_rx.recv() => {
                match reload {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Lagged is fine — even one missed notification still
                        // means "rules changed", so we always recompute.
                        reeval_access_rights(&mut state, &writer).await?;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Sender dropped — the server is going away.
                        break;
                    }
                }
            }
            read = read_with_optional_timeout(&mut reader, &mut buf, inactivity) => {
                match read {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => return Err(e.into()),
                    Err(idle) => {
                        // Inactivity timeout — close the connection.
                        // Disabled by default (matches C rsrv); fires only
                        // when EPICS_CAS_INACTIVITY_TMO is set explicitly.
                        tracing::warn!(
                            target: "epics_ca_rs::server",
                            peer = %state.peer,
                            idle_secs = idle.as_secs(),
                            "CA server: client idle, closing"
                        );
                        break;
                    }
                }
            }
        };
        if n == 0 {
            break;
        }

        // Chaos: optional stall + simulated read drop. Compiles to a
        // single branch when EPICS_CA_RS_CHAOS is unset.
        if crate::chaos::enabled() {
            crate::chaos::maybe_stall().await;
            if crate::chaos::should_drop_read() {
                continue;
            }
        }

        accumulated.extend_from_slice(&buf[..n]);

        // DoS guard: a malformed or hostile client could declare a huge
        // postsize and stream nothing more, growing this Vec unbounded.
        if accumulated.len() > MAX_ACCUMULATED {
            eprintln!(
                "CA server: client accumulated buffer exceeded {} bytes, closing",
                MAX_ACCUMULATED
            );
            break;
        }

        let mut offset = 0;
        while offset + CaHeader::SIZE <= accumulated.len() {
            let (hdr, hdr_size) = CaHeader::from_bytes_extended(&accumulated[offset..])?;
            let actual_post = hdr.actual_postsize();
            let padded_post = align8(actual_post);
            let msg_len = hdr_size + padded_post;

            if offset + msg_len > accumulated.len() {
                break;
            }

            let payload = if actual_post > 0 {
                accumulated[offset + hdr_size..offset + hdr_size + actual_post].to_vec()
            } else {
                Vec::new()
            };

            // Rate-limit gate: drop messages when the bucket is empty;
            // disconnect the client once it accumulates enough strikes.
            if let Some(ref limiter) = state.rate_limiter {
                if limiter.try_acquire().is_err() {
                    metrics::counter!("ca_server_rate_limit_drops_total").increment(1);
                    state.rate_limit_strikes = state.rate_limit_strikes.saturating_add(1);
                    if state.rate_limit_strike_threshold > 0
                        && state.rate_limit_strikes >= state.rate_limit_strike_threshold
                    {
                        tracing::warn!(peer = %state.peer, strikes = state.rate_limit_strikes,
                            "rate limit exceeded; closing connection");
                        metrics::counter!("ca_server_rate_limit_disconnects_total").increment(1);
                        state.audit("disconnect", "", "", "rate_limited").await;
                        return Ok(());
                    }
                    offset += msg_len;
                    continue;
                } else if state.rate_limit_strikes > 0 {
                    state.rate_limit_strikes = 0;
                }
            }

            // Wrap dispatch in send_timeout so a stuck-reader client
            // (kernel send buffer full → `write_all` Pending forever)
            // can be detected and disconnected. Without this, one
            // misbehaving client could deadlock its own per-client
            // task indefinitely. On timeout we drop the connection;
            // any in-flight reply is discarded.
            match tokio::time::timeout(
                send_timeout(),
                dispatch_message(
                    &hdr,
                    &payload,
                    &mut state,
                    &db,
                    &writer,
                    peer,
                    conn_events.as_ref(),
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // Regression defence: dispatch_message no longer
                    // flushes per response (batched at the bottom of
                    // this outer loop). On a propagated dispatch
                    // error, exit-via-`?` would drop the BufWriter
                    // before the outer flush fires, so any responses
                    // queued by earlier successful handlers in this
                    // batch — or by an error-path `send_cmd_error`
                    // call inside the failing handler — would be
                    // lost. Best-effort flush before propagating so
                    // the client sees them; ignore errors here
                    // because the underlying TCP is most likely
                    // already broken (which is why dispatch failed).
                    let _ = writer.lock().await.flush().await;
                    return Err(e);
                }
                Err(_) => {
                    // send_timeout fires — dispatch_message future is
                    // cancelled mid-flight. BufWriter may hold a
                    // partial frame (e.g., header without payload if
                    // cancellation landed between the two write_alls
                    // of a READ_NOTIFY response). Flushing here would
                    // ship the orphan header to the client and leave
                    // it parsing an incomplete frame, so we skip the
                    // flush and let BufWriter drop discard the
                    // partial bytes — same behaviour as before the
                    // batch-flush refactor.
                    tracing::warn!(
                        peer = %peer,
                        "CA server: dispatch send-timeout (stuck client?), closing"
                    );
                    state.audit("disconnect", "", "", "send_timeout").await;
                    return Ok(());
                }
            }
            offset += msg_len;
        }

        if offset > 0 {
            accumulated.drain(..offset);
            // Batched flush: dispatch_message buffered all responses for
            // this read iteration into BufWriter without flushing. Flush
            // once now so the kernel sees a single TCP write per inbound
            // burst. Cuts e2e_bulk_get_many(100) from ~225µs → batched
            // single write (server-side throughput floor was ~2.2µs/PV
            // due to per-message flush; this collapses it to one syscall).
            //
            // Errors here mean the TCP write stalled / peer closed —
            // surface as the read loop's normal disconnect path.
            let mut w = writer.lock().await;
            if let Err(e) = w.flush().await {
                return Err(e.into());
            }
            drop(w);
        }
    }

    // Cleanup: cancel all subscriptions
    for (_, sub) in state.subscriptions.drain() {
        sub.task.abort();
        match &sub.target {
            ChannelTarget::SimplePv(pv) => {
                pv.remove_subscriber(sub.sub_id).await;
            }
            ChannelTarget::RecordField { record, .. } => {
                record.write().await.remove_subscriber(sub.sub_id);
            }
        }
    }

    // Abort any in-flight WRITE_NOTIFY completion tasks (CR-3). A
    // stuck async record (motor hung, asyn device unresponsive) would
    // otherwise hold the spawned task and its captured writer Arc
    // forever after the client disconnects.
    for handle in state.write_notify_tasks.drain(..) {
        handle.abort();
    }

    // Emit a `ChannelCleared` event for every channel still open at
    // disconnect time. Without this, a client that drops without
    // sending `CA_PROTO_CLEAR_CHANNEL` (TCP RST, network drop, panic)
    // leaks its channel refcount in any consumer that uses these
    // events for refcounting (e.g. ca_gateway's per-PV `Active` →
    // `Inactive` transition). Done here so the events fire BEFORE
    // the listener emits `Disconnected(peer)`, preserving the
    // ordering invariant "clears precede disconnect".
    if let Some(tx) = &conn_events {
        for (_sid, entry) in state.channels.drain() {
            let _ = tx.send(ServerConnectionEvent::ChannelCleared {
                peer,
                pv_name: entry.pv_name,
                cid: entry.cid,
            });
        }
    }

    state.audit("disconnect", "", "", "ok").await;
    Ok(())
}

async fn dispatch_message<W: AsyncWrite + Unpin + Send + 'static>(
    hdr: &CaHeader,
    payload: &[u8],
    state: &mut ClientState,
    db: &Arc<PvDatabase>,
    writer: &Arc<Mutex<BufWriter<W>>>,
    peer: SocketAddr,
    conn_events: Option<&broadcast::Sender<ServerConnectionEvent>>,
) -> CaResult<()> {
    match hdr.cmmd {
        CA_PROTO_VERSION => {
            state.client_minor_version = hdr.count;
            let mut resp = CaHeader::new(CA_PROTO_VERSION);
            resp.data_type = 1;
            resp.count = CA_MINOR_VERSION;
            resp.cid = 1;
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_HOST_NAME => {
            // EPICS_CAS_USE_HOST_NAMES (default NO) controls whether we
            // trust the client-supplied hostname for ACF matching. When NO,
            // the peer IP set during accept() is authoritative.
            let trust_client_hostname =
                epics_base_rs::runtime::env::get_or("EPICS_CAS_USE_HOST_NAMES", "NO")
                    .eq_ignore_ascii_case("YES");
            if trust_client_hostname {
                let end = payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(payload.len());
                let claimed = String::from_utf8_lossy(&payload[..end]).to_string();

                // Forward-DNS verification: resolve the client-supplied
                // hostname back to IPs and require one of them to match
                // the actual peer address. Without this check a hostile
                // client could spoof an arbitrary hostname (e.g. that
                // of a privileged operator console) and gain whatever
                // ACF rights the ACL grants to that host. C rsrv has
                // historically deferred this verification to operators
                // (relying on USE_HOST_NAMES=NO in untrusted networks);
                // we fail closed here for stricter defaults.
                let verified = host_resolves_to_peer(&claimed, peer.ip()).await;
                if verified {
                    state.hostname = claimed;
                    // Re-evaluate access rights for all existing channels
                    reeval_access_rights(state, writer).await?;
                } else {
                    tracing::warn!(
                        peer = %peer,
                        claimed_host = %claimed,
                        "CAS_USE_HOST_NAMES: forward-DNS mismatch, ignoring HOST_NAME"
                    );
                    state.audit("host_name", "", &claimed, "dns_mismatch").await;
                    // Keep state.hostname as the peer IP fallback set
                    // at accept(); ACL rules continue to evaluate
                    // against the IP rather than the spoofed hostname.
                }
            }
        }

        CA_PROTO_CLIENT_NAME => {
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let raw = String::from_utf8_lossy(&payload[..end]).to_string();
            // When a capability-token verifier is configured AND the
            // payload arrives in `cap:<token>` form, verify the token
            // and store the resolved subject. Unverifiable tokens are
            // logged and replaced with an `unverified:` sentinel that
            // ACF rules can deliberately deny. Plain (non-`cap:`)
            // usernames pass through unchanged for backwards compat.
            #[cfg(feature = "cap-tokens")]
            {
                // M1: TokenVerifier::verify expects the full `cap:`-
                // prefixed form (it strips the prefix internally).
                // The previous double-strip yielded MissingPrefix on
                // every well-formed token; cap-tokens was non-
                // functional whenever a verifier was configured.
                state.username = match (&state.cap_token_verifier, raw.starts_with("cap:")) {
                    (Some(v), true) => match v.verify(&raw) {
                        Ok(claims) => {
                            tracing::debug!(peer = %state.peer, sub = %claims.sub,
                                "cap-token verified");
                            claims.sub
                        }
                        Err(e) => {
                            tracing::warn!(peer = %state.peer, error = %e,
                                "cap-token verification failed");
                            format!("unverified:{}", &raw)
                        }
                    },
                    _ => raw,
                };
            }
            #[cfg(not(feature = "cap-tokens"))]
            {
                state.username = raw;
            }
            // Re-evaluate access rights for all existing channels
            reeval_access_rights(state, writer).await?;
        }

        CA_PROTO_CREATE_CHAN => {
            // Pre-CA-4.4 clients send claims with no PV name (postsize=0).
            // Silently ignore these, matching C server behavior (camessage.c:1204).
            // The client will retry with v4.4+ format after receiving our VERSION.
            if hdr.actual_postsize() <= 1 {
                return Ok(());
            }

            // DoS guard: refuse new channels once the per-client cap is hit.
            let cap = max_channels_per_client();
            // Pre-warning at 90% — fired once per crossing, not once per
            // CREATE_CHAN, to avoid log spam.
            let warn_threshold = (cap * 9) / 10;
            if !state.channel_limit_warned && state.channels.len() >= warn_threshold {
                tracing::warn!(
                    channels = state.channels.len(),
                    cap,
                    "approaching per-client channel limit (90%)"
                );
                metrics::counter!("ca_server_channel_limit_warnings_total").increment(1);
                state.channel_limit_warned = true;
            }
            if state.channels.len() >= cap {
                tracing::warn!(
                    channels = state.channels.len(),
                    cap,
                    "rejecting CREATE_CHAN: per-client channel limit reached"
                );
                metrics::counter!("ca_server_channel_limit_rejects_total").increment(1);
                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                fail.cid = hdr.cid;
                let mut w = writer.lock().await;
                w.write_all(&fail.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
                return Ok(());
            }

            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();
            let client_cid = hdr.cid;
            let (_base, field_raw) = parse_pv_name(&pv_name);
            let field = field_raw.to_ascii_uppercase();

            if let Some(entry) = db.find_entry(&pv_name).await {
                let sid = state.alloc_sid();

                let (dbr_type, element_count, target) = match entry {
                    PvEntry::Simple(pv) => {
                        let value = pv.get().await;
                        (
                            value.dbr_type(),
                            value.count() as u32,
                            ChannelTarget::SimplePv(pv),
                        )
                    }
                    PvEntry::Record(rec) => {
                        let instance = rec.read().await;
                        // Use resolve_field for 3-level priority
                        let value = instance.resolve_field(&field);
                        match value {
                            Some(v) => {
                                // For waveform records, get_field("VAL") returns
                                // NORD elements (valid data) but the channel's
                                // native count must be NELM (max capacity) so
                                // clients allocate the right buffer.
                                let element_count = if field == "VAL"
                                    && instance.record.record_type() == "waveform"
                                {
                                    instance
                                        .resolve_field("NELM")
                                        .and_then(|n| match n {
                                            EpicsValue::Long(n) => Some(n.max(0) as u32),
                                            _ => None,
                                        })
                                        .unwrap_or(v.count() as u32)
                                } else {
                                    v.count() as u32
                                };
                                (
                                    v.dbr_type(),
                                    element_count,
                                    ChannelTarget::RecordField {
                                        record: rec.clone(),
                                        field: field.clone(),
                                    },
                                )
                            }
                            None => {
                                // Field not found — send CREATE_CH_FAIL
                                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                                fail.cid = client_cid;
                                let mut w = writer.lock().await;
                                w.write_all(&fail.to_bytes()).await?;
                                // flush deferred to handle_client outer loop (batched)
                                return Ok(());
                            }
                        }
                    }
                };

                let access = state.compute_access(&target).await;
                let access_level = match access {
                    3 => AccessLevel::ReadWrite,
                    1 => AccessLevel::Read,
                    _ => AccessLevel::NoAccess,
                };

                state.channels.insert(
                    sid,
                    ChannelEntry {
                        target,
                        cid: client_cid,
                        pv_name: pv_name.clone(),
                    },
                );
                state.channel_access.insert(sid, access_level);

                let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
                ar.cid = client_cid;
                ar.available = access;

                let mut resp = CaHeader::new(CA_PROTO_CREATE_CHAN);
                resp.data_type = dbr_type as u16;
                resp.cid = client_cid;
                resp.available = sid;
                resp.set_payload_size(0, element_count);

                let mut w = writer.lock().await;
                w.write_all(&ar.to_bytes()).await?;
                w.write_all(&resp.to_bytes_extended()).await?;
                // flush deferred to handle_client outer loop (batched)
                drop(w);

                let result = match access_level {
                    AccessLevel::NoAccess => "denied",
                    _ => "ok",
                };
                state.audit("create_chan", &pv_name, "", result).await;

                // Notify subscribers (e.g. ca_gateway tracking PV → client
                // attachments for `Active`/`Inactive` state transitions).
                // `cid` is included so consumers can refcount per
                // (peer, pv_name, cid) — same client opening N channels
                // to the same PV must increment N times.
                if let Some(tx) = &conn_events {
                    let _ = tx.send(ServerConnectionEvent::ChannelCreated {
                        peer,
                        pv_name: pv_name.clone(),
                        cid: client_cid,
                    });
                }
            } else {
                // PV not found — send CREATE_CH_FAIL
                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                fail.cid = client_cid;
                let mut w = writer.lock().await;
                w.write_all(&fail.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
                drop(w);

                state.audit("create_chan", &pv_name, "", "not_found").await;
            }
        }

        CA_PROTO_READ | CA_PROTO_READ_NOTIFY => {
            let is_notify = hdr.cmmd == CA_PROTO_READ_NOTIFY;
            let sid = hdr.cid;
            let ioid = hdr.available;
            let requested_type = hdr.data_type;
            let requested_count = hdr.actual_count();

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            ECA_BADCHID,
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };

            // R38-G2 / Round 38, Round 44 type-state:
            // `state.lookup_access(sid)` is the only path to the
            // access cache. `require_read()` returns a witness on
            // success and an `AccessDenied` carrying the matching
            // ECA code on failure — no `if access ==` ad-hoc
            // comparison, no missing-entry default to argue about.
            let _read_grant = match state.lookup_access(sid).require_read() {
                Ok(g) => g,
                Err(denied) => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            denied.eca_code(),
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };

            let snapshot = get_full_snapshot(&entry.target).await;
            let Some(mut snapshot) = snapshot else {
                if is_notify {
                    send_cmd_error(
                        writer,
                        CA_PROTO_READ_NOTIFY,
                        requested_type,
                        ECA_BADCHID,
                        ioid,
                    )
                    .await?;
                }
                return Ok(());
            };
            // Respect client's requested element count (e.g. caget -# 10)
            if requested_count > 0 && requested_count < snapshot.value.count() {
                snapshot.value.truncate(requested_count as usize);
            }

            // For DBR_STSACK_STRING populate ackt/acks from the record so
            // alarm-handler clients see the current acknowledge state.
            if requested_type == epics_base_rs::types::DBR_STSACK_STRING {
                if let ChannelTarget::RecordField { record, .. } = &entry.target {
                    let inst = record.read().await;
                    if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKT") {
                        snapshot.alarm.ackt = Some(v as u16);
                    }
                    if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKS") {
                        snapshot.alarm.acks = Some(v as u16);
                    }
                }
            }

            // For DBR_CLASS_NAME (38) substitute the record's recordType
            // into the response. SimplePv channels have no record-type
            // identity so they receive an empty string (which matches
            // what the C IOC does for in-process DBR_CLASS_NAME reads
            // against synthetic channels).
            if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                if let ChannelTarget::RecordField { record, .. } = &entry.target {
                    let inst = record.read().await;
                    snapshot.class_name = Some(inst.record.record_type().to_string());
                }
            }

            let data = match encode_dbr(requested_type, &snapshot) {
                Ok(d) => d,
                Err(_) => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };
            // CA-268: DBR_CLASS_NAME wire payload is always one fixed
            // 40-byte string. element_count must be 1 regardless of
            // the underlying record's value count — for waveform
            // records, snapshot.value.count() can be N, which would
            // make C clients parse 40 * N bytes of body and fail.
            let element_count = if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                1
            } else {
                snapshot.value.count() as u32
            };
            let mut padded = data;
            padded.resize(align8(padded.len()), 0);

            // For deprecated CA_PROTO_READ (cmd=3), the response carries the
            // SID in cid (not ECA status). Notify clients (cmd=15) get the
            // ECA_NORMAL status so they can demultiplex by ioid.
            let mut resp = if is_notify {
                let mut r = CaHeader::new(CA_PROTO_READ_NOTIFY);
                r.cid = ECA_NORMAL;
                r
            } else {
                let mut r = CaHeader::new(CA_PROTO_READ);
                r.cid = sid;
                r
            };
            // C client TCP parser requires 8-byte aligned postsize
            resp.set_payload_size(padded.len(), element_count);
            resp.data_type = requested_type;
            resp.available = ioid;

            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes_extended()).await?;
            w.write_all(&padded).await?;
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_WRITE | CA_PROTO_WRITE_NOTIFY => {
            let sid = hdr.cid;
            let ioid = hdr.available;
            let is_notify = hdr.cmmd == CA_PROTO_WRITE_NOTIFY;

            // DBR_PUT_ACKT (35) and DBR_PUT_ACKS (36) are alarm-acknowledge
            // writes — payload is a single u16 routed to the record's
            // ACKT/ACKS field. Handle before the regular DbFieldType
            // dispatch so we don't reject the type as unsupported.
            if hdr.data_type == epics_base_rs::types::DBR_PUT_ACKT
                || hdr.data_type == epics_base_rs::types::DBR_PUT_ACKS
            {
                let entry = match state.channels.get(&sid) {
                    Some(e) => e,
                    None => {
                        if is_notify {
                            send_cmd_error(
                                writer,
                                CA_PROTO_WRITE_NOTIFY,
                                hdr.data_type,
                                ECA_BADCHID,
                                ioid,
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                };
                // R39-G1 / Round 39: alarm-acknowledge PUTs travel
                // the same WRITE wire opcodes but pre-fix bypassed
                // the access_rights check that the regular WRITE
                // path performs below. ACKT/ACKS mutate alarm-handler
                // state — a `NoAccess` peer could silence alarms on
                // any record they could open. Mirror the regular
                // WRITE gate.
                // R39-G1 / Round 44 type-state: alarm-ack PUTs go
                // through the same gate as regular WRITE. Token's
                // `require_write` returns the matching ECA code on
                // denial.
                let _write_grant = match state.lookup_access(sid).require_write() {
                    Ok(g) => g,
                    Err(denied) => {
                        if is_notify {
                            let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                            resp.data_type = hdr.data_type;
                            resp.count = hdr.count;
                            resp.cid = denied.eca_code();
                            resp.available = ioid;
                            let mut w = writer.lock().await;
                            w.write_all(&resp.to_bytes()).await?;
                        }
                        return Ok(());
                    }
                };
                let value_u16 = if payload.len() >= 2 {
                    u16::from_be_bytes([payload[0], payload[1]])
                } else {
                    0
                };
                let field_name = if hdr.data_type == epics_base_rs::types::DBR_PUT_ACKT {
                    "ACKT"
                } else {
                    "ACKS"
                };
                let result = match &entry.target {
                    ChannelTarget::RecordField { record, .. } => {
                        let name = record.read().await.name.clone();
                        db.put_record_field_from_ca(
                            &name,
                            field_name,
                            EpicsValue::Short(value_u16 as i16),
                        )
                        .await
                        .map(|_| ())
                    }
                    ChannelTarget::SimplePv(_) => Err(epics_base_rs::error::CaError::Protocol(
                        "PUT_ACKT/PUT_ACKS only valid on record-backed channels".to_string(),
                    )),
                };
                if is_notify {
                    let eca = match result {
                        Ok(()) => ECA_NORMAL,
                        Err(_) => ECA_PUTFAIL,
                    };
                    let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                    resp.data_type = hdr.data_type;
                    resp.count = hdr.count;
                    resp.cid = eca;
                    resp.available = ioid;
                    let mut w = writer.lock().await;
                    w.write_all(&resp.to_bytes()).await?;
                    // flush deferred to handle_client outer loop (batched)
                }
                return Ok(());
            }

            let write_type = match DbFieldType::from_u16(hdr.data_type) {
                Ok(t) => t,
                Err(_) => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_WRITE_NOTIFY,
                            hdr.data_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_WRITE_NOTIFY,
                            hdr.data_type,
                            ECA_BADCHID,
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };

            // Resolve the audit-friendly PV name once. Cheap when audit
            // is off because state.audit() is a single None check.
            let audit_pv = match &entry.target {
                ChannelTarget::SimplePv(pv) => pv.name.clone(),
                ChannelTarget::RecordField { record, field } => {
                    format!("{}.{}", record.read().await.name, field)
                }
            };

            // Round 44 type-state WRITE gate. `lookup_access` is
            // the only path to the cache; the witness type ensures
            // the matching ECA code reaches the wire.
            let _write_grant = match state.lookup_access(sid).require_write() {
                Ok(g) => g,
                Err(denied) => {
                    if is_notify {
                        let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                        resp.data_type = write_type as u16;
                        resp.count = hdr.count;
                        resp.cid = denied.eca_code();
                        resp.available = ioid;
                        let mut w = writer.lock().await;
                        w.write_all(&resp.to_bytes()).await?;
                    }
                    state.audit("caput", &audit_pv, "", "denied").await;
                    return Ok(());
                }
            };

            let count = hdr.actual_count() as usize;
            let write_count = hdr.count; // Echo back in response (matches C EPICS)
            let new_value = match EpicsValue::from_bytes_array(write_type, payload, count) {
                Ok(v) => v,
                Err(_) => {
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_WRITE_NOTIFY,
                            hdr.data_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };

            // Stringify the value once for the audit log; skipped when
            // audit is off. Use the truncated renderer so a malicious
            // peer can't pin the dispatch task on `format!`-ing a
            // peer-controlled array of millions of elements.
            let audit_value = if state.audit.is_some() {
                new_value.display_truncated(64)
            } else {
                String::new()
            };

            let write_result = match &entry.target {
                ChannelTarget::SimplePv(pv) => {
                    if let Some(hook) = pv.write_hook() {
                        let ctx = epics_base_rs::server::pv::WriteContext {
                            user: state.username.clone(),
                            host: state.hostname.clone(),
                            peer: state.peer.clone(),
                        };
                        hook(new_value, ctx).await.map(|()| None)
                    } else {
                        pv.set(new_value).await;
                        Ok(None)
                    }
                }
                ChannelTarget::RecordField { record, field } => {
                    let name = record.read().await.name.clone();
                    db.put_record_field_from_ca(&name, field, new_value).await
                }
            };

            let audit_result = if write_result.is_ok() { "ok" } else { "fail" };
            state
                .audit("caput", &audit_pv, &audit_value, audit_result)
                .await;

            // F1: CA_PROTO_WRITE (cmd=4) is fire-and-forget — no response
            if is_notify {
                let eca_status = match &write_result {
                    Ok(_) => ECA_NORMAL,
                    Err(e) => e.to_eca_status(),
                };

                // If async processing started (e.g. motor move), spawn a
                // background task to await completion and send the response.
                // This avoids blocking the client handler loop, which would
                // freeze all camonitor subscriptions on this connection.
                let completion_rx: Option<tokio::sync::oneshot::Receiver<()>> =
                    write_result.unwrap_or_default();

                if let Some(rx) = completion_rx {
                    let writer_c = writer.clone();
                    let join = tokio::spawn(async move {
                        // Wait indefinitely for record processing to complete,
                        // matching C EPICS rsrv behavior. RecvError means the
                        // Sender was dropped without firing — typically because
                        // record processing aborted. Surface as ECA_PUTFAIL so
                        // the client doesn't observe a false success.
                        let final_status = match rx.await {
                            Ok(()) => eca_status,
                            Err(_) => ECA_PUTFAIL,
                        };

                        let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                        resp.data_type = write_type as u16;
                        resp.count = write_count;
                        resp.cid = final_status;
                        resp.available = ioid;

                        let mut w = writer_c.lock().await;
                        let _ = w.write_all(&resp.to_bytes()).await;
                        let _ = w.flush().await;
                    });
                    // Track for connection-scoped cleanup (CR-3): a stuck
                    // async record would otherwise pin this task and the
                    // captured writer Arc forever after the client drops.
                    // Reap finished handles opportunistically so the Vec
                    // doesn't grow unbounded over a long-lived connection
                    // that issues many WRITE_NOTIFYs (F1).
                    state.write_notify_tasks.retain(|h| !h.is_finished());
                    state.write_notify_tasks.push(join.abort_handle());
                } else {
                    // Synchronous completion — respond immediately
                    let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                    resp.data_type = write_type as u16;
                    resp.count = write_count;
                    resp.cid = eca_status;
                    resp.available = ioid;

                    let mut w = writer.lock().await;
                    w.write_all(&resp.to_bytes()).await?;
                    // flush deferred to handle_client outer loop (batched)
                }
            }
        }

        CA_PROTO_EVENT_ADD => {
            let sid = hdr.cid;
            let sub_id = hdr.available;
            let requested_type = hdr.data_type;

            // DoS guard: cap subscriptions per channel.
            let subs_for_channel = state
                .subscriptions
                .values()
                .filter(|s| s.channel_sid == sid)
                .count();
            if subs_for_channel >= max_subs_per_channel() {
                send_cmd_error(
                    writer,
                    CA_PROTO_EVENT_ADD,
                    requested_type,
                    ECA_ALLOCMEM,
                    sub_id,
                )
                .await?;
                return Ok(());
            }

            let native_type = match native_type_for_dbr(requested_type) {
                Ok(t) => t,
                Err(_) => {
                    send_cmd_error(
                        writer,
                        CA_PROTO_EVENT_ADD,
                        requested_type,
                        ECA_BADTYPE,
                        sub_id,
                    )
                    .await?;
                    return Ok(());
                }
            };

            let mask = if payload.len() >= 14 {
                u16::from_be_bytes([payload[12], payload[13]])
            } else {
                DBE_VALUE | DBE_ALARM
            };

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    send_cmd_error(
                        writer,
                        CA_PROTO_EVENT_ADD,
                        requested_type,
                        ECA_BADCHID,
                        sub_id,
                    )
                    .await?;
                    return Ok(());
                }
            };

            // R38-G3 / Round 38: EVENT_ADD must also consult the
            // channel's access_rights. A NoAccess peer mounting a
            // subscription would receive every value update —
            // identical leak to the round-32A `subscribe_raw` ACF
            // bypass on the PVA side. C IOC's `event_add_NoAccess`
            // returns ECA_NORDACCESS for the same reason.
            // Round 44 type-state EVENT_ADD gate. R38-G3 closed the
            // missing per-op check; the typed `require_read` shape
            // is the path every future MONITOR-class op should
            // mirror.
            let _read_grant = match state.lookup_access(sid).require_read() {
                Ok(g) => g,
                Err(denied) => {
                    send_cmd_error(
                        writer,
                        CA_PROTO_EVENT_ADD,
                        requested_type,
                        denied.eca_code(),
                        sub_id,
                    )
                    .await?;
                    return Ok(());
                }
            };

            // Refuse a duplicate sub_id on the same connection. Without
            // this, two EVENT_ADDs with identical sub_id leave both
            // subscribers attached to the producer (push without
            // dedup); EVENT_CANCEL strips both at once via retain, but
            // until then every event delivery emits two wire frames —
            // archived data + dashboard counts duplicated.
            if state.subscriptions.contains_key(&sub_id) {
                tracing::warn!(
                    sub_id,
                    "EVENT_ADD refused: sub_id already in use on this connection"
                );
                send_cmd_error(
                    writer,
                    CA_PROTO_EVENT_ADD,
                    requested_type,
                    ECA_BADMONID,
                    sub_id,
                )
                .await?;
                return Ok(());
            }
            {
                match &entry.target {
                    ChannelTarget::SimplePv(pv) => {
                        let rx_opt = pv.add_subscriber(sub_id, native_type, mask).await;
                        let Some(rx) = rx_opt else {
                            // C-G14: per-PV subscriber cap reached.
                            // Round 12: previously dropped silently
                            // (let the client time out). Now sends
                            // ECA_ALLOCMEM so the client surfaces the
                            // refusal immediately and can fall back to
                            // a different transport, retry strategy,
                            // or operator alert. Mirrors the
                            // already-existing per-channel-cap response
                            // a few lines above — same ECA code, same
                            // shape.
                            tracing::warn!(
                                pv = %pv.name,
                                sub_id,
                                "EVENT_ADD refused: PV subscriber cap reached"
                            );
                            send_cmd_error(
                                writer,
                                CA_PROTO_EVENT_ADD,
                                requested_type,
                                ECA_ALLOCMEM,
                                sub_id,
                            )
                            .await?;
                            return Ok(());
                        };

                        // Send initial value
                        let snap = pv.snapshot().await;
                        send_monitor_snapshot(writer, sub_id, requested_type, &snap).await?;

                        let task = spawn_monitor_sender(
                            pv.clone(),
                            sub_id,
                            requested_type,
                            writer.clone(),
                            state.flow_control.clone(),
                            rx,
                        );

                        state.subscriptions.insert(
                            sub_id,
                            SubscriptionEntry {
                                target: ChannelTarget::SimplePv(pv.clone()),
                                channel_sid: sid,
                                sub_id,
                                data_type: requested_type,
                                task,
                            },
                        );
                    }
                    ChannelTarget::RecordField { record, field } => {
                        let mut instance = record.write().await;
                        let Some(rx) = instance.add_subscriber(field, sub_id, native_type, mask)
                        else {
                            // C-G15: record-field subscriber cap reached.
                            // Symmetric with C-G14 (SimplePv path); send
                            // ECA_ALLOCMEM so the client surfaces the
                            // refusal instead of timing out silently.
                            tracing::warn!(
                                record = %instance.name,
                                field = %field,
                                sub_id,
                                "EVENT_ADD refused: record-field subscriber cap reached"
                            );
                            drop(instance);
                            send_cmd_error(
                                writer,
                                CA_PROTO_EVENT_ADD,
                                requested_type,
                                ECA_ALLOCMEM,
                                sub_id,
                            )
                            .await?;
                            return Ok(());
                        };

                        // Send initial value with full metadata
                        if let Some(mut snap) = instance.snapshot_for_field(field) {
                            // CA-268 monitor parity: GET path on
                            // DBR_CLASS_NAME populates class_name from
                            // record_type; the EVENT_ADD initial must
                            // do the same so the first frame carries
                            // the expected string instead of an empty
                            // 40-byte pad.
                            if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                                snap.class_name = Some(instance.record.record_type().to_string());
                            }
                            send_monitor_snapshot(writer, sub_id, requested_type, &snap).await?;
                        }

                        let writer_clone = writer.clone();
                        let flow_control = state.flow_control.clone();
                        let record_for_task = record.clone();
                        let task = epics_base_rs::runtime::task::spawn(async move {
                            let mut rx = rx;
                            loop {
                                // Drain any coalesced overflow value before
                                // blocking on the channel — the producer
                                // parks the latest value here when the mpsc
                                // is full so we always converge on current.
                                let coalesced_opt =
                                    record_for_task.read().await.pop_coalesced(sub_id);
                                let next = if let Some(ev) = coalesced_opt {
                                    Some(ev)
                                } else {
                                    rx.recv().await
                                };
                                let Some(mut event) = next else { break };
                                if flow_control.is_paused() {
                                    let Some(coalesced) =
                                        flow_control.coalesce_while_paused(&mut rx, event).await
                                    else {
                                        break;
                                    };
                                    event = coalesced;
                                }
                                // CA-268 monitor parity: populate
                                // class_name on every emitted event so
                                // a `ca_create_subscription` against
                                // DBR_CLASS_NAME sees the record_type
                                // string instead of an empty 40-byte
                                // pad.
                                if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                                    event.snapshot.class_name = Some(
                                        record_for_task
                                            .read()
                                            .await
                                            .record
                                            .record_type()
                                            .to_string(),
                                    );
                                }
                                let payload_bytes =
                                    match encode_dbr(requested_type, &event.snapshot) {
                                        Ok(bytes) => bytes,
                                        Err(_) => break,
                                    };
                                // CA-268: see GET path note — fixed 1.
                                let element_count =
                                    if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                                        1
                                    } else {
                                        event.snapshot.value.count() as u32
                                    };
                                let mut padded = payload_bytes;
                                padded.resize(align8(padded.len()), 0);

                                let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
                                // C client TCP parser requires 8-byte aligned postsize
                                hdr.set_payload_size(padded.len(), element_count);
                                hdr.data_type = requested_type;
                                hdr.cid = 1; // ECA_NORMAL
                                hdr.available = sub_id;

                                let hdr_bytes = hdr.to_bytes_extended();
                                let mut w = writer_clone.lock().await;
                                if w.write_all(&hdr_bytes).await.is_err() {
                                    break;
                                }
                                if w.write_all(&padded).await.is_err() {
                                    break;
                                }
                                let _ = w.flush().await;
                            }
                        });

                        state.subscriptions.insert(
                            sub_id,
                            SubscriptionEntry {
                                target: ChannelTarget::RecordField {
                                    record: record.clone(),
                                    field: field.clone(),
                                },
                                channel_sid: sid,
                                sub_id,
                                data_type: requested_type,
                                task,
                            },
                        );
                    }
                }
            }
        }

        CA_PROTO_EVENT_CANCEL => {
            let sub_id = hdr.available;
            if let Some(sub) = state.subscriptions.remove(&sub_id) {
                sub.task.abort();
                match &sub.target {
                    ChannelTarget::SimplePv(pv) => {
                        pv.remove_subscriber(sub.sub_id).await;
                    }
                    ChannelTarget::RecordField { record, .. } => {
                        record.write().await.remove_subscriber(sub.sub_id);
                    }
                }

                // Per spec: send final EVENT_ADD response with count=0
                let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
                resp.data_type = sub.data_type;
                resp.count = 0;
                resp.cid = ECA_NORMAL;
                resp.available = sub_id;
                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
            }
        }

        CA_PROTO_EVENTS_OFF | CA_PROTO_EVENTS_ON => {
            if hdr.cmmd == CA_PROTO_EVENTS_OFF {
                state.flow_control.pause();
            } else {
                state.flow_control.resume();
            }
        }

        CA_PROTO_READ_SYNC => {
            // READ_SYNC is a barrier/flush for previously queued responses.
            // No-op here: handle_client's outer-loop flush always fires
            // after the inner message-drain at offset > 0, so any
            // responses buffered before this READ_SYNC will be flushed
            // at the same point as if we had flushed inline.
        }

        CA_PROTO_ECHO => {
            let resp = CaHeader::new(CA_PROTO_ECHO);
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_SEARCH => {
            // TCP search — only supported for clients with minor version >= 4
            if state.client_minor_version < 4 {
                return Ok(());
            }
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();

            if db.has_name(&pv_name).await {
                // Reply: data_type = tcp_port, cid = 0 (INADDR_ANY), available = client's cid
                // 8-byte payload containing CA_MINOR_VERSION as u16
                let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                resp.data_type = state.tcp_port;
                resp.set_payload_size(8, 0);
                resp.cid = 0; // INADDR_ANY — client uses TCP peer addr
                resp.available = hdr.available;

                let mut search_payload = [0u8; 8];
                search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());

                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes_extended()).await?;
                w.write_all(&search_payload).await?;
                // flush deferred to handle_client outer loop (batched)
            } else if hdr.data_type == CA_DO_REPLY {
                // Explicit negative reply requested — send NOT_FOUND so the
                // client doesn't have to wait for a search timeout.
                let mut nf = CaHeader::new(CA_PROTO_NOT_FOUND);
                nf.data_type = CA_DO_REPLY;
                nf.count = CA_MINOR_VERSION;
                nf.cid = hdr.available;
                nf.available = hdr.available;
                let mut w = writer.lock().await;
                w.write_all(&nf.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
            }
            // Otherwise silent — clients without CA_DO_REPLY treat absence
            // as "this server doesn't have it" and move on.
        }

        CA_PROTO_CLEAR_CHANNEL => {
            let sid = hdr.cid;
            let cid = hdr.available;
            if let Some(entry) = state.channels.remove(&sid) {
                state.channel_access.remove(&sid);
                state.release_sid(sid);
                if let Some(tx) = &conn_events {
                    let _ = tx.send(ServerConnectionEvent::ChannelCleared {
                        peer,
                        pv_name: entry.pv_name.clone(),
                        cid: entry.cid,
                    });
                }

                // Clean up subscriptions that belong to this channel
                let sub_ids: Vec<u32> = state
                    .subscriptions
                    .iter()
                    .filter(|(_, sub)| sub.channel_sid == sid)
                    .map(|(&id, _)| id)
                    .collect();
                for sub_id in sub_ids {
                    if let Some(sub) = state.subscriptions.remove(&sub_id) {
                        sub.task.abort();
                        match &sub.target {
                            ChannelTarget::SimplePv(pv) => {
                                pv.remove_subscriber(sub.sub_id).await;
                            }
                            ChannelTarget::RecordField { record, .. } => {
                                record.write().await.remove_subscriber(sub.sub_id);
                            }
                        }
                    }
                }

                let mut resp = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
                resp.data_type = hdr.data_type;
                resp.count = hdr.count;
                resp.cid = sid;
                resp.available = cid;
                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
            }
        }

        _ => {
            // Unknown command — send CA_PROTO_ERROR with ECA status and original header
            let error_msg = format!("Unsupported command {}", hdr.cmmd);
            send_ca_error(writer, hdr, ECA_INTERNAL, &error_msg).await?;
        }
    }

    Ok(())
}
async fn get_full_snapshot(
    target: &ChannelTarget,
) -> Option<epics_base_rs::server::snapshot::Snapshot> {
    match target {
        ChannelTarget::SimplePv(pv) => Some(pv.snapshot().await),
        ChannelTarget::RecordField { record, field } => {
            record.read().await.snapshot_for_field(field)
        }
    }
}

async fn send_monitor_snapshot<W: AsyncWrite + Unpin + Send + 'static>(
    writer: &Arc<Mutex<BufWriter<W>>>,
    sub_id: u32,
    data_type: u16,
    snapshot: &epics_base_rs::server::snapshot::Snapshot,
) -> CaResult<()> {
    let data = encode_dbr(data_type, snapshot)?;
    // CA-268: DBR_CLASS_NAME wire payload is always one 40-byte
    // string regardless of underlying value count.
    let element_count = if data_type == epics_base_rs::types::DBR_CLASS_NAME {
        1
    } else {
        snapshot.value.count() as u32
    };
    let mut padded = data;
    padded.resize(align8(padded.len()), 0);

    let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
    // C client TCP parser requires 8-byte aligned postsize
    resp.set_payload_size(padded.len(), element_count);
    resp.data_type = data_type;
    resp.cid = 1; // ECA_NORMAL
    resp.available = sub_id;

    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes_extended()).await?;
    w.write_all(&padded).await?;
    w.flush().await?;
    Ok(())
}

/// Re-evaluate and re-send CA_PROTO_ACCESS_RIGHTS for all open channels.
/// Called when hostname or username changes.
///
/// Round-39 (R39-G2): when a sid's new access flips to `NoAccess`,
/// any subscriptions currently mounted on that sid must be torn
/// down. Pre-fix the reeval only updated `channel_access` (and
/// notified the client) — active EVENT_ADD subscribers kept
/// receiving every update on the now-denied channel until the
/// client noticed the access drop and issued EVENT_CANCEL. The C
/// IOC cancels subscriptions in the same situation (see
/// `cas_access_rights_change_callback`).
async fn reeval_access_rights<W: AsyncWrite + Unpin + Send + 'static>(
    state: &mut ClientState,
    writer: &Arc<Mutex<BufWriter<W>>>,
) -> CaResult<()> {
    if state.channels.is_empty() {
        return Ok(());
    }
    let chan_info: Vec<(u32, u32, ChannelTarget)> = state
        .channels
        .iter()
        .map(|(&sid, entry)| (sid, entry.cid, entry.target.clone()))
        .collect();

    let mut denied_sids: Vec<u32> = Vec::new();
    {
        let mut w = writer.lock().await;
        for (sid, cid, target) in chan_info {
            let new_access = state.compute_access(&target).await;
            let new_level = match new_access {
                3 => AccessLevel::ReadWrite,
                1 => AccessLevel::Read,
                _ => AccessLevel::NoAccess,
            };
            state.channel_access.insert(sid, new_level);
            if new_level == AccessLevel::NoAccess {
                denied_sids.push(sid);
            }
            let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
            ar.cid = cid;
            ar.available = new_access;
            w.write_all(&ar.to_bytes()).await?;
        }
        w.flush().await?;
    }

    // Tear down every subscription rooted in a now-denied channel.
    if !denied_sids.is_empty() {
        let revoked: Vec<u32> = state
            .subscriptions
            .iter()
            .filter(|(_, s)| denied_sids.contains(&s.channel_sid))
            .map(|(&id, _)| id)
            .collect();
        for sub_id in revoked {
            if let Some(sub) = state.subscriptions.remove(&sub_id) {
                sub.task.abort();
                match &sub.target {
                    ChannelTarget::SimplePv(pv) => {
                        pv.remove_subscriber(sub.sub_id).await;
                    }
                    ChannelTarget::RecordField { record, .. } => {
                        record.write().await.remove_subscriber(sub.sub_id);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Send a command-specific zero-payload error response.
/// Used for READ_NOTIFY, WRITE_NOTIFY, and EVENT_ADD error replies.
async fn send_cmd_error<W: AsyncWrite + Unpin + Send + 'static>(
    writer: &Arc<Mutex<BufWriter<W>>>,
    cmd: u16,
    data_type: u16,
    eca_status: u32,
    ioid_or_subid: u32,
) -> CaResult<()> {
    let mut resp = CaHeader::new(cmd);
    resp.data_type = data_type;
    resp.count = 0;
    resp.cid = eca_status;
    resp.available = ioid_or_subid;
    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes()).await?;
    // flush deferred to handle_client outer loop (batched)
    Ok(())
}

/// Send a CA_PROTO_ERROR response with the original header and an error message.
async fn send_ca_error<W: AsyncWrite + Unpin + Send + 'static>(
    writer: &Arc<Mutex<BufWriter<W>>>,
    original_hdr: &CaHeader,
    eca_status: u32,
    message: &str,
) -> CaResult<()> {
    let error_msg_bytes = pad_string(message);
    let payload_size = CaHeader::SIZE + error_msg_bytes.len();

    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    resp.set_payload_size(payload_size, 0);
    resp.cid = eca_status;

    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes_extended()).await?;
    w.write_all(&original_hdr.to_bytes()).await?;
    w.write_all(&error_msg_bytes).await?;
    // flush deferred to handle_client outer loop (batched)
    Ok(())
}
