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
    /// `CA_PROTO_EVENT_ADD` accepted; a new subscription is live.
    /// Drives `ServerStats::subscriptions_opened_total` (PR #592's
    /// `caServerSubscriptionCount`).
    SubscriptionOpened {
        peer: SocketAddr,
        pv_name: String,
        sub_id: u32,
    },
    /// `CA_PROTO_EVENT_CANCEL` or channel teardown closed a
    /// subscription. Drives `ServerStats::subscriptions_closed_total`.
    /// Subtract from the opened counter for the live subscription
    /// count.
    SubscriptionClosed {
        peer: SocketAddr,
        pv_name: String,
        sub_id: u32,
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
    /// Raw channel-filter JSON suffix the client appended after the
    /// record (epics-base 3.15.7). `None` for ordinary channels;
    /// `Some` when the client requested `REC.{"dbnd":{"d":0.5}}`
    /// etc. Parsed via
    /// `server::database::filters::parse_filter_chain` on
    /// `CA_PROTO_EVENT_ADD` so the filter chain attaches to the
    /// fresh subscriber.
    filter_suffix: Option<String>,
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
    /// Authentication method for ACF `METHOD()` clause matching.
    /// `"x509"` for mTLS-authenticated peers (epics-base PR #641);
    /// `"ca"` (or empty for backwards compat) for plaintext peers.
    /// ACF rules without a `METHOD()` clause ignore this field
    /// — the legacy `check_access_asl()` codepath continues to work.
    auth_method: String,
    /// Authority for ACF `AUTHORITY()` clause matching. mTLS peers
    /// carry their cert's *issuer* DN here so rules like
    /// `RULE(1, WRITE) { METHOD("x509") AUTHORITY("CN=ops-ca, …") }`
    /// can pin write access to certs minted by a specific CA.
    /// Empty for plaintext peers.
    auth_authority: String,
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
    /// Pending WRITE_NOTIFY completion tasks. Each entry is the channel
    /// `sid`-tagged AbortHandle of a task awaiting `put_notify_tx` for
    /// an async record write. Aborted on connection drop so a stuck
    /// async device doesn't leak the task forever, and also aborted
    /// when the owning channel is freed via `CA_PROTO_CLEAR_CHANNEL`
    /// (C parity: `clear_channel_reply` calls `rsrvFreePutNotify`
    /// per-channel — `camessage.c:1889`). The sid tag lets us drain
    /// only the channel-scoped tasks on CLEAR_CHANNEL without
    /// disturbing other channels' in-flight WRITE_NOTIFYs.
    write_notify_tasks: Vec<(u32, tokio::task::AbortHandle)>,
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
            auth_method: String::new(),
            auth_authority: String::new(),
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
                    // PR #641: pass auth method/authority so
                    // METHOD("x509") / AUTHORITY(<issuer>) rules
                    // can gate mTLS-authenticated peers.
                    match acf_cfg.check_access_method(
                        "DEFAULT",
                        &self.hostname,
                        &self.username,
                        0,
                        &self.auth_method,
                        &self.auth_authority,
                    ) {
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
                    // PR #641: pass auth method/authority so
                    // mTLS-only rules (METHOD("x509"), AUTHORITY(...))
                    // can gate write access by issuer CA.
                    let asg = &instance.common.asg;
                    let asl = instance.common.asl;
                    acf_cfg.check_access_method(
                        asg,
                        &self.hostname,
                        &self.username,
                        asl,
                        &self.auth_method,
                        &self.auth_authority,
                    )
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
    // PR #592 dbServerStats: per-connection byte counters feed the
    // `casr` iocsh command's `bytes in=… out=…` line. Optional so unit
    // tests of the TCP path don't need a full ServerStats wired up.
    stats: Option<Arc<super::ca_server::ServerStats>>,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<
        Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>,
    >,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()> {
    // C-G11 (R11): honour every interface in `EPICS_CAS_INTF_ADDR_LIST`,
    // not just the first. C `rsrv_init` (caservertask.c:603-712) iterates
    // `casIntfAddrList` and spawns one `CAS-TCP` accept thread per
    // entry, all bound to the same TCP port. Binding to a *specific*
    // interface IP (vs `INADDR_ANY`) and binding to a *different*
    // specific IP on the same port is allowed by POSIX; only two
    // 0.0.0.0 binds collide. Empty list → single `0.0.0.0` listener
    // (default), preserving the current single-NIC behaviour.
    //
    // First successful bind decides `actual_port` (honouring the
    // existing AddrInUse → ephemeral-fallback path). All subsequent
    // binds must use that same port; if a per-interface bind fails
    // it is logged and skipped (matches C `cleanup:` / `continue;` in
    // `caservertask.c:744-749`, which frees the conf and proceeds).
    let intf_addrs: Vec<std::net::Ipv4Addr> = {
        let cfg = super::addr_list::from_env();
        if cfg.intf_addrs.is_empty() {
            vec![std::net::Ipv4Addr::UNSPECIFIED]
        } else {
            cfg.intf_addrs
        }
    };

    let mut listeners: Vec<(TcpListener, std::net::Ipv4Addr)> = Vec::new();
    let mut actual_port: Option<u16> = None;
    for ip in &intf_addrs {
        let target_port = actual_port.unwrap_or(port);
        let bind_ip = std::net::IpAddr::V4(*ip);
        let listener = match TcpListener::bind((bind_ip, target_port)).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && actual_port.is_none() => {
                // Only fall back to ephemeral on the FIRST bind. Once
                // a port has been chosen all subsequent interfaces
                // MUST share it — otherwise a multi-NIC server would
                // advertise a different TCP port per interface in its
                // SEARCH replies (which already carry a single port).
                TcpListener::bind((bind_ip, 0)).await?
            }
            Err(e) => {
                if actual_port.is_some() {
                    // Subsequent bind on chosen port failed: log and
                    // skip (C parity, `cleanup:` path frees + continues).
                    tracing::warn!(
                        target: "epics_ca_rs::server::tcp",
                        intf = %ip,
                        port = target_port,
                        error = %e,
                        "TCP listener bind failed on this interface — skipping"
                    );
                    continue;
                }
                return Err(e.into());
            }
        };
        let chosen = listener.local_addr()?.port();
        if actual_port.is_none() {
            actual_port = Some(chosen);
        }
        listeners.push((listener, *ip));
    }

    let actual_port = match actual_port {
        Some(p) => p,
        None => {
            // C `cantProceed("CAS: No TCP server started\n")` at
            // `caservertask.c:752`. Every configured interface failed
            // to bind — there's nothing to serve.
            return Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "CAS: No TCP server started — all configured interfaces failed to bind",
            )));
        }
    };
    let _ = tcp_port_tx.send(actual_port);

    // One accept-loop task per bound interface. When the parent
    // `run_tcp_listener` future is dropped (CaServer shutdown via
    // `tcp_abort.abort()`), this JoinSet is dropped which aborts all
    // accept loops as a unit. First task to error wins; the rest
    // are aborted via JoinSet::Drop.
    let mut accept_tasks: tokio::task::JoinSet<CaResult<()>> = tokio::task::JoinSet::new();
    for (listener, intf) in listeners {
        let db_t = db.clone();
        let acf_t = acf.clone();
        let acf_reload_tx_t = acf_reload_tx.clone();
        let beacon_reset_t = beacon_reset.clone();
        let conn_events_t = conn_events.clone();
        let audit_t = audit.clone();
        let drain_t = drain.clone();
        let stats_t = stats.clone();
        #[cfg(feature = "experimental-rust-tls")]
        let tls_t = tls.clone();
        #[cfg(feature = "cap-tokens")]
        let cap_token_verifier_t = cap_token_verifier.clone();
        accept_tasks.spawn(async move {
            accept_loop(
                listener,
                intf,
                actual_port,
                db_t,
                acf_t,
                acf_reload_tx_t,
                beacon_reset_t,
                conn_events_t,
                audit_t,
                drain_t,
                stats_t,
                #[cfg(feature = "experimental-rust-tls")]
                tls_t,
                #[cfg(feature = "cap-tokens")]
                cap_token_verifier_t,
            )
            .await
        });
    }

    // Wait for the first error (or for all loops to exit cleanly via
    // drain). On error, JoinSet::Drop aborts the surviving loops.
    while let Some(res) = accept_tasks.join_next().await {
        match res {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => return Err(e),
            Err(join_err) if join_err.is_cancelled() => continue,
            Err(join_err) => {
                return Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    join_err.to_string(),
                )));
            }
        }
    }
    Ok(())
}

/// Per-interface accept loop. Owned by `run_tcp_listener` via a
/// `JoinSet` — one task per `EPICS_CAS_INTF_ADDR_LIST` entry. Drains
/// when the shared `drain` flag is set; otherwise spawns a
/// `handle_client` task into the local `conn_tasks` `JoinSet` per
/// accepted connection.
///
/// `intf` is the bound interface IP; recorded on accept-error logs so
/// multi-NIC hosts can tell which listener saw the failure. The
/// `actual_port` parameter is the TCP port shared across all
/// listeners (decided in `run_tcp_listener`).
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: TcpListener,
    intf: std::net::Ipv4Addr,
    actual_port: u16,
    db: Arc<PvDatabase>,
    acf: Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>,
    acf_reload_tx: broadcast::Sender<()>,
    beacon_reset: std::sync::Arc<tokio::sync::Notify>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    audit: Option<crate::audit::AuditLogger>,
    drain: Arc<std::sync::atomic::AtomicBool>,
    stats: Option<Arc<super::ca_server::ServerStats>>,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<
        Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>,
    >,
    #[cfg(feature = "cap-tokens")] cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
) -> CaResult<()> {
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
            tracing::info!(intf = %intf, "TCP listener: drain mode set, exiting accept loop");
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
        tracing::info!(peer = %peer, intf = %intf, "CA client connected");
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
        let stats_for_client = stats.clone();
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
                                // Extract verified peer identity + issuer
                                // from the client certificate, if presented.
                                let (identity, authority) = tls_stream
                                    .get_ref()
                                    .1
                                    .peer_certificates()
                                    .and_then(|chain| chain.first())
                                    .map(|cert| {
                                        (
                                            crate::tls::identity_from_cert(cert),
                                            crate::tls::issuer_from_cert(cert),
                                        )
                                    })
                                    .map(|(id, auth)| (Some(id), auth))
                                    .unwrap_or((None, None));
                                if let Some(ref id) = identity {
                                    tracing::info!(
                                        peer = %peer,
                                        identity = %id,
                                        authority = authority.as_deref().unwrap_or("<none>"),
                                        "mTLS identity verified"
                                    );
                                }
                                handle_client(
                                    tls_stream,
                                    peer,
                                    db,
                                    acf,
                                    acf_reload_rx,
                                    actual_port,
                                    identity,
                                    authority,
                                    audit,
                                    conn_events.clone(),
                                    stats_for_client.clone(),
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
                            None,
                            audit,
                            conn_events.clone(),
                            stats_for_client.clone(),
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
                        None,
                        audit,
                        conn_events.clone(),
                        stats_for_client.clone(),
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
    // PR #641 — mTLS issuer DN of the peer's cert. `Some(...)` only
    // when the peer was authenticated via mTLS; gets paired with
    // `auth_method = "x509"` on the ClientState so ACF
    // METHOD()/AUTHORITY() rules can gate by issuer.
    tls_authority: Option<String>,
    audit: Option<crate::audit::AuditLogger>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>,
    // PR #592 dbServerStats: bytes_in/bytes_out counters. Incremented
    // post-read (per accepted UDP/TCP buffer) and at each BufWriter
    // flush (by inspecting `BufWriter::buffer().len()` before flush).
    // `None` skips all counter bookkeeping — used by the unit-test
    // dispatch fixtures that don't spin up a full CaServer.
    stats: Option<Arc<super::ca_server::ServerStats>>,
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
    // PR #641: surface the mTLS authentication context to the ACF
    // check. Plaintext peers stay with empty fields — every legacy
    // rule (no METHOD/AUTHORITY clause) ignores them.
    if let Some(authority) = tls_authority {
        state.auth_method = "x509".to_string();
        state.auth_authority = authority;
    }
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

        // PR #592 dbServerStats: bytes_in mirrors RSRV's
        // `caServerBytes_in`. Counted on every successful read of `n`
        // wire bytes, regardless of whether the inner dispatch
        // accepts or rejects the message.
        if let Some(ref s) = stats {
            s.bytes_in
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
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
            // C `rsrv/camessage.c:2452` rejects misaligned payloads
            // ("CAS: Missaligned protocol rejected") with an
            // ECA_INTERNAL error and disconnects the client. Our
            // previous code silently rounded up via `align8`, which on
            // a hostile peer would cause us to read into the next
            // message's header and de-sync the stream. Now: emit
            // CA_PROTO_ERROR + drop the connection (match C).
            if actual_post & 0x7 != 0 {
                tracing::warn!(
                    peer = %state.peer,
                    cmmd = hdr.cmmd,
                    postsize = actual_post,
                    "CAS: Missaligned protocol rejected"
                );
                let _ = send_ca_error(
                    &writer,
                    &hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "CAS: Missaligned protocol rejected",
                )
                .await;
                let _ = writer.lock().await.flush().await;
                return Err(epics_base_rs::error::CaError::Protocol(
                    "misaligned CA payload".into(),
                ));
            }
            let msg_len = hdr_size + actual_post;

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
            // PR #592 dbServerStats: bytes_out mirrors RSRV's
            // `caServerBytes_out`. Capture the buffered size *before*
            // flush so we know exactly how many wire bytes leave on
            // this syscall. CA-over-TLS counts post-decrypt plaintext
            // since the rustls layer wraps the BufWriter externally —
            // matches what the comment on ServerStats::bytes_out
            // already documents.
            let pending_out = w.buffer().len() as u64;
            if let Err(e) = w.flush().await {
                return Err(e.into());
            }
            if let Some(ref s) = stats {
                s.bytes_out
                    .fetch_add(pending_out, std::sync::atomic::Ordering::Relaxed);
            }
            drop(w);
        }
    }

    // Cleanup: cancel all subscriptions. PR #592 dbServerStats —
    // emit `SubscriptionClosed` for each so the running close-count
    // matches the open-count when a client disconnects without
    // explicit EVENT_CANCEL (TCP RST, network drop, panic). Without
    // this, `active_subscriptions` reports a permanent leak after
    // every ungraceful disconnect.
    let pending_subs: Vec<SubscriptionEntry> =
        state.subscriptions.drain().map(|(_, sub)| sub).collect();
    for sub in pending_subs {
        sub.task.abort();
        match &sub.target {
            ChannelTarget::SimplePv(pv) => {
                pv.remove_subscriber(sub.sub_id).await;
            }
            ChannelTarget::RecordField { record, .. } => {
                record.write().await.remove_subscriber(sub.sub_id);
            }
        }
        if let Some(tx) = &conn_events {
            let pv_name = state
                .channels
                .get(&sub.channel_sid)
                .map(|e| e.pv_name.clone())
                .unwrap_or_default();
            let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                peer,
                pv_name,
                sub_id: sub.sub_id,
            });
        }
    }

    // Abort any in-flight WRITE_NOTIFY completion tasks (CR-3). A
    // stuck async record (motor hung, asyn device unresponsive) would
    // otherwise hold the spawned task and its captured writer Arc
    // forever after the client disconnects.
    for (_sid, handle) in state.write_notify_tasks.drain(..) {
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
            // C `tcp_version_action` (camessage.c:366-369): rejects
            // clients whose minor version < CA_MINIMUM_SUPPORTED_VERSION
            // (=4) with RSRV_ERROR, which tears the connection down.
            // Without this gate, an ancient client could complete the
            // VERSION handshake and proceed to CREATE_CHAN with a
            // wire format we no longer fully support — silently
            // diverging from C IOC behaviour.
            const CA_MINIMUM_SUPPORTED_VERSION: u16 = 4;
            if hdr.count < CA_MINIMUM_SUPPORTED_VERSION {
                tracing::warn!(
                    peer = ?peer,
                    minor = hdr.count,
                    "CAS: Ignore version from unsupported client (minor < 4); dropping"
                );
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "unsupported CA minor version {} (matches C tcp_version_action drop)",
                    hdr.count
                )));
            }
            state.client_minor_version = hdr.count;
            // C `rsrv_version_reply` (camessage.c:2115) emits VERSION
            // with all fields zero except `m_count = CA_MINOR_PROTOCOL_REVISION`.
            // The previous Rust defaults (`data_type=1, cid=1`) drifted
            // from byte-exact parity — C clients only consult `m_count`
            // (`tcpiiu.cpp::versionRespNotify`) so it was harmless in
            // practice, but a strict peer or wire trace would diverge.
            let mut resp = CaHeader::new(CA_PROTO_VERSION);
            resp.count = CA_MINOR_VERSION;
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_HOST_NAME => {
            // C `camessage.c::host_name_action` (line ~795 onward)
            // rejects HOST_NAME messages that arrive after the first
            // channel has been created — once the client claims any
            // channel, the host identity is fixed for the connection.
            // Reuse the same wire response: CA_PROTO_ERROR with
            // ECA_INTERNAL and a descriptive message.
            if !state.channels.is_empty() {
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "attempts to use protocol to set host name \
                     after creating first channel ignored by server",
                )
                .await?;
                return Ok(());
            }
            // C `camessage.c:824-825`: `size = strnlen(pName, m_postsize)
            // + 1; if (size > 512 || size > m_postsize) reject`.
            // Our `end` here is the string length (null position or
            // payload end), so `end + 1 > 512` ⇔ `end >= 512`.
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            if end >= 512 {
                // C `host_name_action` (camessage.c:825-836): a name
                // longer than 511 bytes is a protocol violation —
                // send_err + return RSRV_ERROR (disconnect). The
                // post-claim freeze branch above returns RSRV_OK
                // (recoverable misuse), but the size cap is a
                // wire-malformation reject.
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "bad (very long) host name",
                )
                .await?;
                return Err(epics_base_rs::error::CaError::Protocol(
                    "HOST_NAME exceeds 511-byte cap (matches C host_name_action RSRV_ERROR)".into(),
                ));
            }

            // EPICS_CAS_USE_HOST_NAMES (default NO) controls whether we
            // trust the client-supplied hostname for ACF matching. When NO,
            // the peer IP set during accept() is authoritative.
            let trust_client_hostname =
                epics_base_rs::runtime::env::get_or("EPICS_CAS_USE_HOST_NAMES", "NO")
                    .eq_ignore_ascii_case("YES");
            if trust_client_hostname {
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
            // C `camessage.c::client_name_action` rejects CLIENT_NAME
            // after the first channel has been created (line ~898).
            if !state.channels.is_empty() {
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "attempts to use protocol to set user name \
                     after creating first channel ignored by server",
                )
                .await?;
                return Ok(());
            }
            // C `camessage.c:911-912`: same 512-byte cap as host name.
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            if end >= 512 {
                // C `client_name_action` (camessage.c:912-923): same
                // 511-byte cap as host_name; send_err + RSRV_ERROR
                // (disconnect). Post-claim freeze branch returns
                // RSRV_OK; size cap returns RSRV_ERROR.
                send_ca_error(
                    writer,
                    hdr,
                    ECA_INTERNAL,
                    0xFFFF_FFFF,
                    "a very long user name was specified",
                )
                .await?;
                return Err(epics_base_rs::error::CaError::Protocol(
                    "CLIENT_NAME exceeds 511-byte cap (matches C client_name_action RSRV_ERROR)"
                        .into(),
                ));
            }
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
                // C parity: `claim_ciu_action` (rsrv/camessage.c:1229-1239)
                // routes channel-allocation failure through
                // `send_err(mp, ECA_ALLOCMEM, …)`, NOT
                // CREATE_CH_FAIL. CREATE_CH_FAIL is reserved for the
                // `dbChannel_create` (PV/field not found) branch
                // (camessage.c:1212-1219). libca
                // `exceptionRespAction` surfaces the ECA_ALLOCMEM
                // status to the user-level callback so the client
                // knows "server out of resources" vs CREATE_CH_FAIL's
                // "PV does not exist on this server" — the existing
                // Rust path conflated the two, leading clients to
                // remove our address from their resolution cache on
                // a transient server saturation. Per `vsend_err`'s
                // switch, CA_PROTO_CREATE_CHAN falls to `default`
                // and uses `0xffffffff` for `m_cid`.
                send_ca_error(writer, hdr, ECA_ALLOCMEM, u32::MAX, "channel limit reached").await?;
                // C `claim_ciu_action` (camessage.c:1229-1240): when
                // the server's channel-allocation pool is exhausted,
                // send_err(ECA_ALLOCMEM) is followed by RSRV_ERROR
                // which tears the connection down. The Rust per-
                // client cap is the closest analogue: same root
                // cause (this client requested more channels than
                // the server is willing to hold) and the same
                // ECA_ALLOCMEM wire byte. Match C by dropping the
                // connection so a misbehaving client doesn't sit
                // and spam CREATE_CHAN frames against a saturated
                // cap; the next reconnect re-baselines.
                return Err(epics_base_rs::error::CaError::Protocol(
                    "CREATE_CHAN per-client cap reached \
                     (matches C claim_ciu_action ECA_ALLOCMEM + RSRV_ERROR)"
                        .into(),
                ));
            }

            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();
            let client_cid = hdr.cid;
            // epics-base 3.15.7 channel-filter suffix
            // (`REC.{"dbnd":{"d":0.5}}`). Split the JSON suffix off
            // for the record lookup, but keep `pv_name` verbatim so
            // the audit log and `ChannelCreated`/`ChannelCleared`
            // events still surface the literal string the client
            // used. `filter_suffix` is stashed on the channel so
            // EVENT_ADD can build a `FilterChain` from it later.
            let parsed_channel =
                epics_base_rs::server::database::filters::split_channel_name(&pv_name);
            let record_path = parsed_channel.record_path;
            let filter_suffix = parsed_channel.json_suffix;
            let (_base, field_raw) = parse_pv_name(&record_path);
            let field = field_raw.to_ascii_uppercase();

            if let Some(entry) = db.find_entry(&record_path).await {
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
                        filter_suffix: filter_suffix.clone(),
                    },
                );
                state.channel_access.insert(sid, access_level);

                let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
                ar.cid = client_cid;
                ar.available = access;

                // C `claim_ciu_reply` (camessage.c:1157-1167): clients
                // whose minor version is below CA_V49 (= 9) cannot parse
                // extended-form headers. For those peers, `nElem` is
                // capped at 0xfffe so the CREATE_CHAN reply stays in
                // normal-form (16-byte) layout; V4.9+ clients receive
                // the true count via the extended header.
                let nelem = if state.client_minor_version < 9 && element_count >= 0xffff {
                    0xfffe
                } else {
                    element_count
                };
                let mut resp = CaHeader::new(CA_PROTO_CREATE_CHAN);
                resp.data_type = dbr_type as u16;
                resp.cid = client_cid;
                resp.available = sid;
                resp.set_payload_size(0, nelem);

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
                    // C `read_action` (camessage.c:616-620) checks
                    // `INVALID_DB_REQ(m_dataType)` (type >
                    // LAST_BUFFER_TYPE = 38) BEFORE any DB lookup and
                    // returns `RSRV_ERROR` which tears the connection
                    // down. Other read-path failures (cas_copy_in_header
                    // budget, access denied, dbChannel_get, caNetConvert
                    // host-net) all return RSRV_OK and keep the
                    // connection — those don't apply here.
                    //
                    // Rust `encode_dbr` failure with `UnsupportedType`
                    // is the direct parallel of INVALID_DB_REQ —
                    // emit the error + drop the connection.
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    } else {
                        send_ca_error(writer, hdr, ECA_BADTYPE, hdr.cid, "bad READ data type")
                            .await?;
                    }
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "READ with unsupported DBR type {} (matches C read_action RSRV_ERROR)",
                        requested_type
                    )));
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

            // For deprecated CA_PROTO_READ (cmd=3), the response carries
            // the *client-side* CID (`pciu->cid` in C `read_action`
            // — `camessage.c:622-624` passes `pciu->cid`, NOT
            // `pciu->sid`, to `cas_copy_in_header`). Modern libca's
            // `readRespAction` demuxes by ioid (`m_available`) and
            // ignores `m_cid` for READ responses, but pre-3.14 clients
            // and stricter wire validators (Wireshark CA dissector,
            // packet-level fuzzers) cross-check the field. Notify
            // clients (cmd=15) get ECA_NORMAL since READ_NOTIFY's cid
            // slot carries status, not the channel CID.
            let mut resp = if is_notify {
                let mut r = CaHeader::new(CA_PROTO_READ_NOTIFY);
                r.cid = ECA_NORMAL;
                r
            } else {
                let mut r = CaHeader::new(CA_PROTO_READ);
                r.cid = entry.cid;
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
                    // C `write_notify_action` (camessage.c:1647-1651) and
                    // `write_action` (camessage.c:753-766) both treat
                    // unsupported data types as a protocol violation:
                    // emit the appropriate error reply, then return
                    // RSRV_ERROR which tears the connection down. The
                    // C source classifies this as "client doesn't
                    // recover" — a peer sending an unsupported DBR
                    // either has a corrupted dispatcher or is probing
                    // for protocol weaknesses; either way the right
                    // response is to drop.
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_WRITE_NOTIFY,
                            hdr.data_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    } else {
                        send_ca_error(writer, hdr, ECA_BADTYPE, hdr.cid, "bad data type").await?;
                    }
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "WRITE with unsupported DBR type {} (matches C write_action RSRV_ERROR)",
                        hdr.data_type
                    )));
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
                    } else {
                        // C `write_action` (`rsrv/camessage.c:741-750`)
                        // emits `send_err(mp, ECA_NOWTACCESS, client,
                        // RECORD_NAME(pciu->dbch))` even for the no-
                        // notify WRITE. Without this branch the Rust
                        // server dropped denied PROTO_WRITEs silently —
                        // C libca's `cac::exception` path never fired,
                        // so a `caput` from a read-only peer looked
                        // like it had succeeded (no error callback)
                        // even though the value never reached the DB.
                        send_ca_error(writer, hdr, denied.eca_code(), hdr.cid, &audit_pv).await?;
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
                    // Same C parity rule as the data_type gate above:
                    // bad payload bytes (wrong length, malformed wire
                    // bytes) is a protocol violation → emit error +
                    // drop the connection. C `caNetConvert` failure
                    // in `write_action` returns RSRV_ERROR.
                    if is_notify {
                        send_cmd_error(
                            writer,
                            CA_PROTO_WRITE_NOTIFY,
                            hdr.data_type,
                            ECA_BADTYPE,
                            ioid,
                        )
                        .await?;
                    } else {
                        send_ca_error(writer, hdr, ECA_BADTYPE, hdr.cid, "bad WRITE payload bytes")
                            .await?;
                    }
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "WRITE payload conversion failed for type {} count {} (matches C caNetConvert RSRV_ERROR)",
                        hdr.data_type, count
                    )));
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
                    // that issues many WRITE_NOTIFYs (F1). The `sid` tag
                    // also lets `CA_PROTO_CLEAR_CHANNEL` drain only the
                    // tasks owned by the cleared channel (C parity:
                    // `rsrvFreePutNotify` per-channel cleanup).
                    state.write_notify_tasks.retain(|(_, h)| !h.is_finished());
                    state.write_notify_tasks.push((sid, join.abort_handle()));
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
                    // C `event_add_action` (camessage.c:1769-1771):
                    // `INVALID_DB_REQ` (data_type > LAST_BUFFER_TYPE = 38)
                    // returns RSRV_ERROR with NO error reply — the
                    // connection just drops. Unlike WRITE / READ where
                    // C emits CA_PROTO_ERROR + drops, EVENT_ADD is
                    // silent. Match that wire shape: no send, just
                    // disconnect. Clients see EOF without an ECA hint;
                    // this matches C IOC behaviour exactly.
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "EVENT_ADD with unsupported DBR type {} (matches C event_add_action silent drop)",
                        requested_type
                    )));
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
                    // C `event_add_action` (camessage.c:1773-1777):
                    // `logBadId` + RSRV_ERROR on missing channel —
                    // logs server-side, no wire reply, then drops the
                    // connection. Same silent-disconnect pattern as
                    // the INVALID_DB_REQ branch above.
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "EVENT_ADD on unknown SID {} (matches C event_add_action logBadId + RSRV_ERROR)",
                        sid
                    )));
                }
            };
            // Captured up front so the SubscriptionOpened event we
            // emit after a successful insert below doesn't have to
            // re-borrow `state.channels` (the insert path mutates
            // `state.subscriptions` so the entry borrow has to be
            // released before then).
            let sub_pv_name = entry.pv_name.clone();

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
                        if let Some(tx) = conn_events {
                            let _ = tx.send(ServerConnectionEvent::SubscriptionOpened {
                                peer,
                                pv_name: sub_pv_name.clone(),
                                sub_id,
                            });
                        }
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

                        // epics-base 3.15.7 channel filter — if the
                        // channel was created with a `.{...}` JSON
                        // suffix, build the FilterChain now and attach
                        // it to the just-registered subscriber. The
                        // parser is permissive: malformed JSON or
                        // unknown filters degrade gracefully to an
                        // empty chain with a tracing::warn!.
                        if let Some(json) = entry.filter_suffix.as_deref() {
                            let chain =
                                epics_base_rs::server::database::filters::parse_filter_chain(json);
                            for filt in chain.iter() {
                                instance.attach_filter_to_last_subscriber(field, filt.clone());
                            }
                        }

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
                        if let Some(tx) = conn_events {
                            let _ = tx.send(ServerConnectionEvent::SubscriptionOpened {
                                peer,
                                pv_name: sub_pv_name.clone(),
                                sub_id,
                            });
                        }
                    }
                }
            }
        }

        CA_PROTO_EVENT_CANCEL => {
            let sub_id = hdr.available;
            if let Some(sub) = state.subscriptions.remove(&sub_id) {
                sub.task.abort();
                // Resolve pv_name for the SubscriptionClosed event.
                // Look up via the subscription's channel_sid; if the
                // channel was already cleared, fall back to an empty
                // string (the event still increments the counter).
                let pv_name_for_event = state
                    .channels
                    .get(&sub.channel_sid)
                    .map(|e| e.pv_name.clone())
                    .unwrap_or_default();
                match &sub.target {
                    ChannelTarget::SimplePv(pv) => {
                        pv.remove_subscriber(sub.sub_id).await;
                    }
                    ChannelTarget::RecordField { record, .. } => {
                        record.write().await.remove_subscriber(sub.sub_id);
                    }
                }
                if let Some(tx) = conn_events {
                    let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                        peer,
                        pv_name: pv_name_for_event,
                        sub_id,
                    });
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
            } else {
                // C `event_cancel_reply` (`camessage.c:1998-2021`):
                // when the sub-id (m_available of the request) does
                // not match any active subscription on the addressed
                // channel, send `send_err(ECA_BADMONID,
                // RECORD_NAME(pciu->dbch))`. The previous Rust
                // behaviour was a silent ignore, leaving libca-driven
                // tools that race a CLEAR_CHANNEL against an
                // EVENT_CANCEL with a stale sub-id waiting for an
                // exception that never arrives (the stale request
                // was discarded).
                //
                // The diagnostic string uses the resolved PV name
                // when the m_cid in the request still maps to a
                // channel; otherwise we fall back to "unknown"
                // (matches C, which would log via `logBadId` and
                // return RSRV_ERROR — we degrade to a NORMAL reply
                // path with a descriptive diag).
                let req_sid = hdr.cid;
                let (chan_cid, diag) = match state.channels.get(&req_sid) {
                    Some(entry) => (entry.cid, entry.pv_name.clone()),
                    None => (0xFFFF_FFFFu32, "unknown".to_string()),
                };
                tracing::debug!(
                    sub_id,
                    sid = req_sid,
                    "EVENT_CANCEL for unknown sub-id; replying ECA_BADMONID"
                );
                send_ca_error(writer, hdr, ECA_BADMONID, chan_cid, &diag).await?;
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
            // C `read_sync_reply` (camessage.c:2053-2067): server
            // echoes the request header back with cmmd=CA_PROTO_READ_SYNC,
            // m_postsize=0, and the request's m_dataType / m_count /
            // m_cid / m_available preserved. libca client treats this
            // as ECHO (`cac.cpp:72-73`: "legacy READ_SYNC used as
            // echo with legacy server" — dispatched through
            // echoRespAction). Without the reply, a client using
            // READ_SYNC as a keepalive probe (legacy V3 / pre-V4.3
            // protocol behavior) sees no response and may trigger
            // its connection-timeout watchdog.
            //
            // The outer batched flush still fires, so any prior queued
            // responses ship along with this echo — preserving the
            // barrier semantic. Pre-fix Rust silently no-op-ed; this
            // restores wire parity.
            let mut resp = CaHeader::new(CA_PROTO_READ_SYNC);
            resp.data_type = hdr.data_type;
            resp.count = hdr.count;
            resp.cid = hdr.cid;
            resp.available = hdr.available;
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_ECHO => {
            // C `tcp_echo_action` (`rsrv/camessage.c:403-420`) echoes
            // the *full* request back to the client — same m_cmmd,
            // m_postsize, m_dataType, m_count, m_cid, m_available, and
            // the m_postsize-byte payload. Real clients (libca
            // `tcpiiu::echoRequest`) issue zero-payload echos with
            // every field zero, in which case our previous
            // `CaHeader::new(CA_PROTO_ECHO).to_bytes()` happened to be
            // byte-identical to C. But a diagnostic / probe client
            // that sends ECHO with a marker payload (e.g. to measure
            // RTT or to verify the server isn't a TCP transparent
            // proxy) gets a stripped, all-zero reply from us — wire
            // divergence that breaks the documented round-trip
            // semantics.
            let mut resp = CaHeader::new(CA_PROTO_ECHO);
            // Preserve the request fields. set_payload_size handles
            // both the short and extended encodings transparently.
            resp.data_type = hdr.data_type;
            resp.set_payload_size(hdr.actual_postsize(), hdr.actual_count());
            resp.cid = hdr.cid;
            resp.available = hdr.available;
            let mut w = writer.lock().await;
            if resp.is_extended() {
                w.write_all(&resp.to_bytes_extended()).await?;
            } else {
                w.write_all(&resp.to_bytes()).await?;
            }
            // Echo the payload back verbatim (truncated to the actual
            // postsize advertised by the request — `payload` here is
            // already that slice).
            if !payload.is_empty() {
                w.write_all(payload).await?;
            }
            // flush deferred to handle_client outer loop (batched)
        }

        CA_PROTO_SEARCH => {
            // C `search_reply_tcp` (camessage.c:2238-2241): if
            // `!CA_VSUPPORTED(m_count)` (minor < 4) the handler
            // returns RSRV_ERROR which tears the TCP connection
            // down. Note that the *UDP* SEARCH path returns RSRV_OK
            // on the same condition (silently skips the reply, no
            // datagram-level disconnect) — those two paths share
            // the version-check logic but differ in fatality.
            //
            // Pre-fix Rust silently `return Ok(())`-ed, keeping the
            // connection. A peer could spam unsupported-minor TCP
            // SEARCH frames indefinitely.
            if state.client_minor_version < 4 {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "TCP SEARCH from minor {} (< 4) — C search_reply_tcp RSRV_ERROR parity",
                    state.client_minor_version
                )));
            }
            // C `search_reply_tcp` (rsrv/camessage.c:2246) rejects
            // SEARCH whose `m_postsize <= 1` and silently returns
            // RSRV_OK. Mirror that here so an attacker's empty-name
            // SEARCH burst on an open TCP connection cannot drive
            // `db.has_name("")` per frame nor trigger a NOT_FOUND
            // amplification when CA_DO_REPLY is set.
            if hdr.postsize <= 1 {
                return Ok(());
            }
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();

            if db.has_name(&pv_name).await {
                // C parity: `search_reply_tcp`
                // (`rsrv/camessage.c:2229-2287`) sends:
                //   m_postsize  = 0  (no payload — TCP search reply
                //                 carries no minor-version trailer,
                //                 unlike UDP)
                //   m_dataType  = ca_server_port (carries the port)
                //   m_count     = 0
                //   m_cid       = ~0U (INADDR_BROADCAST — tells client
                //                 to use TCP peer addr as server IP;
                //                 libca `tcpiiu::searchRespNotify`
                //                 explicitly checks `msg.m_cid !=
                //                 INADDR_BROADCAST` and falls back to
                //                 `this->address()` on the sentinel)
                //   m_available = client's m_available (the cid)
                //
                // The previous code wrote `m_cid = 0` (INADDR_ANY) and
                // an 8-byte minor-version payload. C libca client at
                // `tcpiiu.cpp:2209` treats anything != INADDR_BROADCAST
                // as a literal IP, so `m_cid = 0` would surface as a
                // server at 0.0.0.0:port — unroutable. With this fix
                // the reply is now byte-equivalent to the C softIoc.
                let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                resp.data_type = state.tcp_port;
                resp.set_payload_size(0, 0);
                resp.cid = u32::MAX; // ~0U — "use TCP peer addr"
                resp.available = hdr.available;

                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                // flush deferred to handle_client outer loop (batched)
            } else if hdr.data_type == CA_DO_REPLY {
                // Explicit negative reply requested — send NOT_FOUND so
                // the client doesn't have to wait for a search timeout.
                //
                // C parity: `search_fail_reply` (rsrv/camessage.c:2079)
                // copies the request's `m_dataType`/`m_count`/`m_cid`/
                // `m_available` verbatim into the response. The previous
                // Rust path overwrote `count` with the server's
                // CA_MINOR_VERSION and `cid` with the request's
                // `m_available` (which happens to equal `m_cid` for
                // libca search frames, but the parity intent is
                // "echo m_cid"). With this fix the reply is byte-
                // equivalent to a C softIoc fail reply.
                let mut nf = CaHeader::new(CA_PROTO_NOT_FOUND);
                nf.data_type = hdr.data_type;
                nf.count = hdr.count;
                nf.cid = hdr.cid;
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
            // C `clear_channel_reply` (camessage.c:1883-1887) silently
            // disconnects on a bad SID via `logBadId` + RSRV_ERROR
            // (no wire reply). Channels in this Rust state are per-
            // client by construction, so the "foreign channel"
            // sub-case of the C check (`pciu->client != client`)
            // can't happen — the only failure mode is unknown SID.
            // Pre-fix Rust silently skipped without disconnecting,
            // so a probing peer could send CLEAR_CHANNEL on random
            // SIDs indefinitely.
            if !state.channels.contains_key(&sid) {
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "CLEAR_CHANNEL on unknown SID {} (matches C clear_channel_reply logBadId + RSRV_ERROR)",
                    sid
                )));
            }
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

                // C parity: `clear_channel_reply` (`camessage.c:1889`)
                // calls `rsrvFreePutNotify` to drain pending PUT_NOTIFY
                // operations for this channel. Without aborting the
                // matching tasks, a stuck async record could later
                // emit a stale WRITE_NOTIFY response carrying the
                // cleared channel's ioid — confusing the client's
                // ioid demultiplex. Drain finished handles
                // opportunistically while iterating.
                drain_write_notify_tasks_for_sid(&mut state.write_notify_tasks, sid);

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
                        if let Some(tx) = &conn_events {
                            let _ = tx.send(ServerConnectionEvent::SubscriptionClosed {
                                peer,
                                pv_name: entry.pv_name.clone(),
                                sub_id,
                            });
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
            // Unknown command — match C `bad_tcp_cmd_action`
            // (`camessage.c:337-352`): send CA_PROTO_ERROR with
            // ECA_INTERNAL and the 0xFFFFFFFF cid sentinel (per
            // `vsend_err` non-channel-scoped convention), then
            // tear down the connection. C returns `RSRV_ERROR`
            // which breaks the dispatcher's message loop
            // (`camessage.c:2519-2524`) — its comment is
            // explicit: "by default, clients don't recover from
            // this". Without the tear-down, a misbehaving or
            // malicious peer can flood the server with unknown
            // commands and force one CA_PROTO_ERROR reply per
            // frame indefinitely.
            let error_msg = format!("Unsupported command {}", hdr.cmmd);
            send_ca_error(writer, hdr, ECA_INTERNAL, 0xFFFF_FFFF, &error_msg).await?;
            return Err(epics_base_rs::error::CaError::Protocol(format!(
                "unsupported TCP command {} (matches C bad_tcp_cmd_action drop)",
                hdr.cmmd
            )));
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

/// Send a CA_PROTO_ERROR response with the original header echoed
/// into the payload and an error message.
///
/// Layout follows C `vsend_err` (`rsrv/camessage.c:139`):
///   * outer `m_cid` carries the *channel client cid* (i.e. the
///     client-side identifier of the channel the error relates to),
///     or `0xFFFFFFFF` for commands that aren't channel-scoped.
///   * outer `m_available` carries the ECA status code.
///   * payload is the original request header followed by a
///     NUL-terminated diagnostic string.
///
/// The previous implementation put the ECA status in `m_cid` and left
/// `m_available` zero, so libca's `exceptionRespAction`
/// (`cac.cpp:1118`) — which reads the status from `hdr.m_available` —
/// would surface every server-emitted CA_PROTO_ERROR as ECA_NORMAL
/// (status 0), silently masking the failure.
/// C `vsend_err` (rsrv/camessage.c:147,229-242) allocates a fixed
/// 512-byte buffer for the entire reply (outer header + echoed
/// request header + diagnostic + NUL), and `epicsVsnprintf` truncates
/// the formatted diagnostic if it would overflow. Mirror that bound
/// so a buggy caller (or future translated message catalog) can't
/// ship a CA_PROTO_ERROR whose payload exceeds the libca per-server
/// recv buffer or the extended-header threshold. 480 = 512 −
/// 2*sizeof(caHdr) matches the diagnostic budget C grants
/// `epicsVsnprintf`.
const CA_PROTO_ERROR_MAX_DIAG_LEN: usize = 480;

/// On `CA_PROTO_CLEAR_CHANNEL`, abort any pending WRITE_NOTIFY
/// completion task whose owning channel `sid` is being freed (C
/// parity: `clear_channel_reply` calls `rsrvFreePutNotify` per
/// channel — `camessage.c:1889`). Finished handles are reaped
/// opportunistically while iterating so the per-connection Vec stays
/// bounded across many WRITE_NOTIFYs over a long-lived connection.
/// Pure transformation extracted so the drain semantics are unit-
/// testable without standing up a full server + async record.
fn drain_write_notify_tasks_for_sid(tasks: &mut Vec<(u32, tokio::task::AbortHandle)>, sid: u32) {
    let mut keep = Vec::with_capacity(tasks.len());
    let mut to_abort = Vec::new();
    for (task_sid, h) in tasks.drain(..) {
        if h.is_finished() {
            continue;
        }
        if task_sid == sid {
            to_abort.push(h);
        } else {
            keep.push((task_sid, h));
        }
    }
    *tasks = keep;
    for h in to_abort {
        h.abort();
    }
}

/// Truncate `message` to at most `CA_PROTO_ERROR_MAX_DIAG_LEN` bytes
/// on a char boundary (so the resulting `&str` slice is always valid
/// UTF-8). `pad_string` appends the NUL terminator and 8-aligns.
fn truncate_diag(message: &str) -> &str {
    if message.len() <= CA_PROTO_ERROR_MAX_DIAG_LEN {
        return message;
    }
    let mut end = CA_PROTO_ERROR_MAX_DIAG_LEN;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

async fn send_ca_error<W: AsyncWrite + Unpin + Send + 'static>(
    writer: &Arc<Mutex<BufWriter<W>>>,
    original_hdr: &CaHeader,
    eca_status: u32,
    chan_cid: u32,
    message: &str,
) -> CaResult<()> {
    let error_msg_bytes = pad_string(truncate_diag(message));
    let payload_size = CaHeader::SIZE + error_msg_bytes.len();

    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    resp.set_payload_size(payload_size, 0);
    resp.cid = chan_cid;
    resp.available = eca_status;

    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes_extended()).await?;
    w.write_all(&original_hdr.to_bytes()).await?;
    w.write_all(&error_msg_bytes).await?;
    // flush deferred to handle_client outer loop (batched)
    Ok(())
}

#[cfg(test)]
mod write_notify_drain_tests {
    use super::drain_write_notify_tasks_for_sid;

    /// Spawn a long-running task (sleep-loop) and return its abort
    /// handle. The handle's `is_finished()` flips to true once `abort()`
    /// has fired AND the runtime has processed the cancellation. We
    /// poll for that transition in the test below — drop-flag
    /// approaches were timing-sensitive on saturated CI runners.
    fn spawn_pending() -> tokio::task::AbortHandle {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .abort_handle()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drains_only_matching_sid() {
        let h_a = spawn_pending();
        let h_b = spawn_pending();
        let h_c = spawn_pending();
        let h_a_probe = h_a.clone();
        let h_b_probe = h_b.clone();
        let h_c_probe = h_c.clone();
        let mut tasks = vec![(10u32, h_a), (20u32, h_b), (10u32, h_c)];

        drain_write_notify_tasks_for_sid(&mut tasks, 10);

        // sid=20 entry survives
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, 20);

        // Wait up to 2s (generous for saturated CI) for the aborted
        // tasks to actually finish. The sid=20 task must still be
        // running (no abort fired against it).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if h_a_probe.is_finished() && h_c_probe.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(h_a_probe.is_finished(), "sid=10 task #1 must be aborted");
        assert!(h_c_probe.is_finished(), "sid=10 task #3 must be aborted");
        assert!(!h_b_probe.is_finished(), "sid=20 task must survive");

        // Cleanup the surviving task so we don't leak.
        h_b_probe.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaps_finished_handles_during_drain() {
        // A handle whose future already completed should be removed
        // from the Vec regardless of whether its sid matches — this
        // is the opportunistic-reap behaviour the long-lived
        // connection relies on.
        let done = tokio::spawn(async {}).abort_handle();
        for _ in 0..200 {
            if done.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(done.is_finished(), "spawned no-op task should complete");

        let live = spawn_pending();
        let live_probe = live.clone();
        let mut tasks = vec![(99u32, done), (5u32, live)];
        drain_write_notify_tasks_for_sid(&mut tasks, 1234);
        assert_eq!(tasks.len(), 1, "finished handle was not reaped");
        assert_eq!(tasks[0].0, 5);

        // Cleanup the still-live task.
        live_probe.abort();
    }
}

#[cfg(test)]
mod truncate_diag_tests {
    use super::{CA_PROTO_ERROR_MAX_DIAG_LEN, truncate_diag};

    #[test]
    fn passes_through_short_message() {
        let s = "channel limit reached";
        assert_eq!(truncate_diag(s), s);
    }

    #[test]
    fn truncates_at_exact_limit() {
        let s = "x".repeat(CA_PROTO_ERROR_MAX_DIAG_LEN);
        assert_eq!(truncate_diag(&s).len(), CA_PROTO_ERROR_MAX_DIAG_LEN);
    }

    #[test]
    fn truncates_oversize_to_limit() {
        let s = "x".repeat(CA_PROTO_ERROR_MAX_DIAG_LEN + 100);
        let out = truncate_diag(&s);
        assert_eq!(out.len(), CA_PROTO_ERROR_MAX_DIAG_LEN);
        assert!(out.chars().all(|c| c == 'x'));
    }

    #[test]
    fn truncation_lands_on_utf8_char_boundary() {
        // Construct a string that crosses the 480-byte cap inside
        // a multi-byte UTF-8 sequence: 'é' (U+00E9) is 2 bytes in
        // UTF-8. Padding with 479 'a's puts the first byte of 'é'
        // exactly at byte 479 — within the limit — and the second
        // at byte 480 — past it. Naive byte slicing would split it
        // and panic. `truncate_diag` must back off to the previous
        // char boundary (byte 479).
        let mut s = "a".repeat(479);
        s.push('é');
        assert_eq!(s.len(), 481);
        let out = truncate_diag(&s);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.len() <= CA_PROTO_ERROR_MAX_DIAG_LEN);
        // Standard library guarantees: the returned &str is valid
        // UTF-8 (otherwise this method-call would panic).
        let _ = out.to_owned();
    }
}

#[cfg(test)]
mod multi_nic_listener_tests {
    //! C-parity regression: `run_tcp_listener` must honour every entry
    //! in `EPICS_CAS_INTF_ADDR_LIST`, not just the first.
    //!
    //! C `rsrv_init` (caservertask.c:603-712) iterates `casIntfAddrList`
    //! and spawns one accept thread per entry, all on the same TCP
    //! port. The previous Rust implementation bound only the first
    //! interface, so a server configured with `INTF_ADDR_LIST="A B"`
    //! silently dropped TCP accepts on interface B.
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio::sync::{Notify, broadcast, oneshot};

    /// Spawn `run_tcp_listener` against a per-test database, return the
    /// (port, abort-handle). Honours whatever EPICS_CAS_INTF_ADDR_LIST
    /// is currently set in the process env (caller manages it).
    async fn start_listener() -> (u16, tokio::task::JoinHandle<()>) {
        let db = Arc::new(PvDatabase::new());
        let acf = Arc::new(tokio::sync::RwLock::new(None));
        let (acf_reload_tx, _) = broadcast::channel::<()>(4);
        let (tcp_tx, tcp_rx) = oneshot::channel::<u16>();
        let beacon_reset = Arc::new(Notify::new());
        let drain = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(async move {
            let _ = run_tcp_listener(
                db,
                0, // ephemeral
                acf,
                acf_reload_tx,
                tcp_tx,
                beacon_reset,
                None,
                None,
                drain,
                None,
                #[cfg(feature = "experimental-rust-tls")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await;
        });
        let port = tcp_rx.await.expect("listener bound");
        (port, handle)
    }

    /// Confirm `INTF_ADDR_LIST=127.0.0.1` results in a listener that
    /// accepts on 127.0.0.1. This is the "single specific IP" path
    /// which already worked pre-R11 — the test guards against a
    /// regression in the refactor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn single_specific_intf_binds_and_accepts() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`; restored before return.
        unsafe { std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1") };

        let (port, listener_task) = start_listener().await;
        // Connect — TCP handshake completes only if the listener bound
        // to 127.0.0.1 and is accepting.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same `serial_test::serial` scope.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST"),
            }
        }
    }

    /// Two-entry `INTF_ADDR_LIST`: the first valid interface decides
    /// the port; the second must bind on the same port. Use
    /// `127.0.0.1` for both — POSIX rejects two identical
    /// (addr,port) binds, so the second bind on the same loopback IP
    /// fails. The R11 contract is that a failed *subsequent* bind is
    /// logged-and-skipped (matching C `cleanup: continue;`), and the
    /// listener as a whole still serves the first interface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn duplicate_intf_subsequent_skipped_not_fatal() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`.
        unsafe { std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1 127.0.0.1") };

        let (port, listener_task) = start_listener().await;
        // First listener still accepts.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded — first listener serves");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same scope.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST"),
            }
        }
    }

    /// Empty list → falls back to single 0.0.0.0 bind (default).
    /// Asserts the empty-list path didn't regress.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn empty_intf_list_binds_wildcard() {
        let saved = std::env::var("EPICS_CAS_INTF_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`.
        unsafe { std::env::remove_var("EPICS_CAS_INTF_ADDR_LIST") };

        let (port, listener_task) = start_listener().await;
        // 0.0.0.0 binds accept connections on every local IP including
        // 127.0.0.1.
        let stream = tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .expect("connect within timeout")
        .expect("connect succeeded");
        drop(stream);

        listener_task.abort();
        let _ = listener_task.await;

        // SAFETY: same scope.
        unsafe {
            if let Some(v) = saved {
                std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", v);
            }
        }
    }
}
