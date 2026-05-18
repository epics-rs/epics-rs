use epics_base_rs::runtime::sync::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Maximum accumulated TCP read buffer per client (DoS guard).
///
/// This MUST be >= the largest legal single frame, otherwise a valid
/// large waveform (e.g. a 2 MB array, well under the 16 MB
/// `max_payload_size()` default) would push `accumulated` past the cap
/// and the connection would be closed before the frame could be
/// dispatched — a permanent failure that survives reconnect.
///
/// Largest legal frame = extended header (24 bytes) + `max_payload_size()`
/// payload. We add a 64 KiB slack so a partially-received *next* frame
/// pipelined behind a full one in the same read burst does not trip the
/// guard before the first frame is drained. `max_payload_size()` honours
/// `EPICS_CA_MAX_ARRAY_BYTES`, so the cap tracks any operator override.
/// Mirrors the client-side cap in `client/transport.rs`.
fn max_accumulated() -> usize {
    crate::protocol::max_payload_size()
        .saturating_add(24)
        .saturating_add(64 * 1024)
}

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
/// (~30s); coordinated peers can tie up listener resources. Default
/// 10 s, override via `EPICS_CAS_TLS_HANDSHAKE_TMO`. Floored at 1s.
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
    /// Gate flipped by `reeval_access_rights` when read access is
    /// revoked / restored for `channel_sid`. While `true`, the
    /// producer task drops events at the send step (matches C
    /// `casAccessRightsCB`, `rsrv/camessage.c:1080-1095`, which
    /// calls `db_event_disable` rather than tearing the
    /// subscription down — so an ACF reload that later restores
    /// access can resume the same camonitor).
    denied: Arc<AtomicBool>,
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
    /// TLS channel binding (SHA-256 of the peer's leaf certificate DER)
    /// for this connection. `Some(..)` only when the peer connected
    /// over mTLS and presented a client certificate; `None` for
    /// plaintext circuits. A cap-token presented over a plaintext
    /// circuit (`None`) is rejected by `TokenVerifier::verify` —
    /// mTLS-gating, so a stolen token cannot be replayed off-channel.
    #[cfg(feature = "cap-tokens")]
    tls_channel_binding: Option<crate::cap_token::ChannelBinding>,
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
            #[cfg(feature = "cap-tokens")]
            tls_channel_binding: None,
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
/// emitter restarts its fast beacon cycle. This is a Rust enhancement, NOT
/// C parity: C `rsrv` resets the beacon interval only on `ctlPause`, never
/// on connect/disconnect. The extra fast beacons are benign and help
/// clients notice server state changes promptly.
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
        // Reap finished connection tasks promptly. The select! arm on
        // `conn_tasks.join_next()` only fires when `listener.accept()`
        // is Pending, but `biased` makes the accept arm strictly
        // preferred — so under a sustained connect storm completed
        // `JoinHandle`s would accumulate in the set unbounded. A
        // non-blocking `try_join_next` drain after every accept caps
        // the set at the count of genuinely in-flight connections.
        while conn_tasks.try_join_next().is_some() {}
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
        // Rust enhancement (NOT C parity): C `rsrv` never resets the
        // beacon interval on connect — only on `ctlPause`. We restart
        // the fast beacon cycle here so clients notice the new server
        // state quickly; the extra beacons are benign.
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
                                let leaf_cert = tls_stream
                                    .get_ref()
                                    .1
                                    .peer_certificates()
                                    .and_then(|chain| chain.first().cloned());
                                let (identity, authority) = leaf_cert
                                    .as_ref()
                                    .map(|cert| {
                                        (
                                            crate::tls::identity_from_cert(cert),
                                            crate::tls::issuer_from_cert(cert),
                                        )
                                    })
                                    .map(|(id, auth)| (Some(id), auth))
                                    .unwrap_or((None, None));
                                // TLS channel binding: SHA-256 of the
                                // peer's leaf certificate DER. Threaded
                                // into `handle_client` so cap-token
                                // verification is bound to this circuit.
                                #[cfg(feature = "cap-tokens")]
                                let tls_channel_binding = leaf_cert.as_ref().map(|cert| {
                                    crate::cap_token::ChannelBinding::from_peer_cert_der(
                                        cert.as_ref(),
                                    )
                                });
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
                                    #[cfg(feature = "cap-tokens")]
                                    tls_channel_binding,
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
                            // Plaintext circuit: no channel binding.
                            #[cfg(feature = "cap-tokens")]
                            None,
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
                        // No TLS compiled in: never a channel binding.
                        #[cfg(feature = "cap-tokens")]
                        None,
                    )
                    .await
                }
            };
            // Rust enhancement (NOT C parity): C `rsrv` never resets
            // the beacon interval on disconnect — only on `ctlPause`.
            // Restarting the fast beacon cycle here is a deliberate,
            // benign addition.
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
    // TLS channel binding (SHA-256 of the peer's leaf cert DER),
    // computed at the mTLS accept site. `None` for plaintext peers —
    // a cap-token presented on a `None` circuit is rejected by
    // `TokenVerifier::verify`, so the token is cryptographically
    // bound to the TLS channel it was issued for.
    #[cfg(feature = "cap-tokens")] tls_channel_binding: Option<crate::cap_token::ChannelBinding>,
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
        state.tls_channel_binding = tls_channel_binding;
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

    // CRITICAL: every exit path from the read loop — graceful EOF
    // (`break`), propagated I/O / protocol error, rate-limit disconnect,
    // send-timeout disconnect — MUST pass through the single teardown
    // block below (subscription cancel, write-notify abort,
    // SubscriptionClosed / ChannelCleared emission). Previously the
    // in-loop `return Ok(())` / `return Err(..)` sites bypassed the
    // teardown, leaking write-notify tasks and inflating consumer
    // refcounts permanently after any non-graceful disconnect.
    //
    // The loop is wrapped in a labeled block: in-loop exits use
    // `break 'client_loop <CaResult>` so control always reaches the
    // teardown, and the captured result is returned only afterwards.
    //
    // `disconnect_reason` carries the specific cause (rate_limited /
    // send_timeout / error / ok) to the single post-teardown audit
    // call — replacing the per-path `state.audit("disconnect", ..)`
    // calls that previously had to live next to each `return`.
    let mut disconnect_reason: &str = "ok";
    let loop_result: CaResult<()> = 'client_loop: {
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
                            // means "rules changed", so we always recompute. A
                            // re-push failure must still pass through teardown.
                            if let Err(e) = reeval_access_rights(&mut state, &writer).await {
                                break 'client_loop Err(e);
                            }
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Sender dropped — the server is going away.
                            break 'client_loop Ok(());
                        }
                    }
                }
                read = read_with_optional_timeout(&mut reader, &mut buf, inactivity) => {
                    match read {
                        Ok(Ok(n)) => n,
                        Ok(Err(e)) => break 'client_loop Err(e.into()),
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
                            break 'client_loop Ok(());
                        }
                    }
                }
            };
            if n == 0 {
                break 'client_loop Ok(());
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
            let accum_cap = max_accumulated();
            if accumulated.len() > accum_cap {
                eprintln!(
                    "CA server: client accumulated buffer exceeded {accum_cap} bytes, closing"
                );
                break 'client_loop Ok(());
            }

            let mut offset = 0;
            while offset + CaHeader::SIZE <= accumulated.len() {
                // C `camessage` dispatcher (camessage.c:2471-2489): if
                // msgsize > maxstk (recv buffer ceiling, =
                // rsrvSizeofLargeBufTCP after expand), emit ECA_TOLARGE
                // via send_err and drain the rest of the message. Rust
                // `CaHeader::from_bytes_extended` returns
                // CaError::Protocol("payload too large") when the
                // extended postsize exceeds `max_payload_size()`
                // (default 16 MiB), and the `?` propagation silently
                // closes the connection. C clients waiting on the
                // ECA_TOLARGE error callback see only EOF. Pre-check
                // the extended postsize here and emit the wire reply
                // before propagating the error.
                //
                // Normal-form headers can't overflow `max_payload_size()`
                // because their postsize is u16 (max 0xfffe < 16 MiB),
                // so the check only triggers on extended frames.
                let buf = &accumulated[offset..];
                if buf.len() >= 24 && buf[2] == 0xFF && buf[3] == 0xFF {
                    let ext_post =
                        u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]) as usize;
                    if ext_post > crate::protocol::max_payload_size() {
                        // Build a stand-in header for the error reply
                        // (cmmd echoed from the malformed frame; cid
                        // sentinel 0xFFFFFFFF per `vsend_err`
                        // non-channel-scoped convention).
                        let mut probe_hdr = CaHeader::new(u16::from_be_bytes([buf[0], buf[1]]));
                        probe_hdr.data_type = u16::from_be_bytes([buf[4], buf[5]]);
                        let _ = send_ca_error(
                            &writer,
                            &probe_hdr,
                            ECA_TOLARGE,
                            0xFFFF_FFFF,
                            "CAS: Server unable to load large request message",
                        )
                        .await;
                        let _ = writer.lock().await.flush().await;
                        break 'client_loop Err(epics_base_rs::error::CaError::Protocol(format!(
                            "CA payload too large: ext_post={} > max={} \
                         (matches C dispatcher ECA_TOLARGE wire reply + drop)",
                            ext_post,
                            crate::protocol::max_payload_size()
                        )));
                    }
                }
                // C `rsrv/camessage.c:~2410`: when the buffer holds a
                // partial extended-form header (16..24 bytes of a message
                // whose `m_postsize == 0xffff`), C does `status = RSRV_OK;
                // break;` to await the remaining bytes — it does NOT
                // disconnect. Without this guard, `from_bytes_extended`
                // returns `Err("extended header incomplete")` and the `?`
                // below closes the connection on a benign TCP segment
                // boundary. The ECA_TOLARGE pre-check above is gated on
                // `buf.len() >= 24`, so it never masks this 16..24 window.
                if buf.len() < 24 && buf[2] == 0xFF && buf[3] == 0xFF {
                    break;
                }
                let (hdr, hdr_size) = match CaHeader::from_bytes_extended(&accumulated[offset..]) {
                    Ok(v) => v,
                    Err(e) => break 'client_loop Err(e),
                };
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
                    break 'client_loop Err(epics_base_rs::error::CaError::Protocol(
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
                            metrics::counter!("ca_server_rate_limit_disconnects_total")
                                .increment(1);
                            disconnect_reason = "rate_limited";
                            break 'client_loop Ok(());
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
                        break 'client_loop Err(e);
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
                        disconnect_reason = "send_timeout";
                        break 'client_loop Ok(());
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
                    break 'client_loop Err(e.into());
                }
                if let Some(ref s) = stats {
                    s.bytes_out
                        .fetch_add(pending_out, std::sync::atomic::Ordering::Relaxed);
                }
                drop(w);
            }
        }
    };

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

    // Audit with the outcome the loop exited on, then return that
    // outcome. The teardown above ran unconditionally regardless of
    // whether `loop_result` is Ok or Err. An Err exit that did not set
    // a more specific reason is reported as "error".
    if loop_result.is_err() && disconnect_reason == "ok" {
        disconnect_reason = "error";
    }
    state.audit("disconnect", "", "", disconnect_reason).await;
    loop_result
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
    // C dispatcher (camessage.c:2427-2440): any non-VERSION command
    // from a client whose minor_version_number is below
    // CA_MINIMUM_SUPPORTED_VERSION (= 4) gets ECA_DEFUNCT via
    // send_err and the message is drained (status = RSRV_OK,
    // connection stays open). The intent is "let new clients
    // identify themselves but tell pre-V4.4 peers they're too old".
    //
    // Rust's `state.client_minor_version` defaults to 0 (set only
    // by the VERSION handler). Pre-fix Rust would dispatch any
    // non-VERSION command on a fresh connection with minor=0,
    // bypassing the gate. The CREATE_CHAN / READ / WRITE wire
    // formats may differ for ancient clients; the C IOC's
    // ECA_DEFUNCT hint lets the client decide whether to upgrade.
    //
    // Note: TCP VERSION with minor<4 already disconnects via
    // round-12 dbb4b28, so this gate only triggers on clients
    // that skipped the VERSION handshake entirely OR on a peer
    // explicitly identifying as pre-V4.4.
    if hdr.cmmd != CA_PROTO_VERSION && state.client_minor_version < 4 {
        send_ca_error(
            writer,
            hdr,
            ECA_DEFUNCT,
            0xFFFF_FFFF,
            "CAS: Client version too old",
        )
        .await?;
        return Ok(());
    }

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
            // C `tcp_version_action` (`rsrv/camessage.c:371-373`) drops
            // the connection (`return RSRV_ERROR`) when the client's
            // requested priority (`m_dataType`) exceeds
            // `CA_PROTO_PRIORITY_MAX` (= 99u in `caProto.h:71`). The
            // priority drives the IOC's per-client epicsThread
            // scheduling-priority assignment downstream, so a value
            // outside the legal 0..=99 range is rejected hard rather
            // than silently clamped. Pre-fix Rust accepted any
            // priority and emitted the VERSION reply normally —
            // benign on the wire but diverges from libca's expected
            // close-on-bad-priority behaviour, which a strict CAC
            // peer would notice.
            const CA_PROTO_PRIORITY_MAX: u16 = 99;
            if hdr.data_type > CA_PROTO_PRIORITY_MAX {
                tracing::warn!(
                    peer = ?peer,
                    priority = hdr.data_type,
                    "CAS: VERSION with priority > CA_PROTO_PRIORITY_MAX; dropping"
                );
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "VERSION priority {} > {} (matches C tcp_version_action drop)",
                    hdr.data_type, CA_PROTO_PRIORITY_MAX
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
            // The second condition rejects payloads with no null
            // terminator within m_postsize bytes (strnlen returns
            // m_postsize, then +1 overflows). Rust's
            // `position(|&b| b == 0)` returns Some(idx) for
            // terminated names and `None` (mapped to payload.len())
            // for unterminated ones — check explicitly so we don't
            // silently accept unterminated names that C would
            // reject as "very long".
            let null_pos = payload.iter().position(|&b| b == 0);
            let end = null_pos.unwrap_or(payload.len());
            if null_pos.is_none() || end >= 512 {
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
            // C `camessage.c:911-912`: same 512-byte cap as host
            // name, AND same null-termination requirement. C
            // computes `size = strnlen(pName, m_postsize) + 1`
            // then rejects on `size > m_postsize`, which catches
            // names with no null terminator within m_postsize
            // bytes. Match by treating "no null found" as a
            // reject.
            let null_pos = payload.iter().position(|&b| b == 0);
            let end = null_pos.unwrap_or(payload.len());
            if null_pos.is_none() || end >= 512 {
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
            // logged and replaced with a fixed `unverified` sentinel
            // that ACF rules can deliberately deny. Plain (non-`cap:`)
            // usernames pass through unchanged for backwards compat.
            #[cfg(feature = "cap-tokens")]
            {
                // M1: TokenVerifier::verify expects the full `cap:`-
                // prefixed form (it strips the prefix internally).
                // The previous double-strip yielded MissingPrefix on
                // every well-formed token; cap-tokens was non-
                // functional whenever a verifier was configured.
                state.username = match (&state.cap_token_verifier, raw.starts_with("cap:")) {
                    (Some(v), true) => match v.verify(&raw, state.tls_channel_binding.as_ref()) {
                        Ok(claims) => {
                            tracing::debug!(peer = %state.peer, sub = %claims.sub,
                                "cap-token verified");
                            claims.sub
                        }
                        Err(e) => {
                            // Do NOT fold the raw token into the username:
                            // it then lands in the ACF identity and the
                            // audit log. A structurally valid but rejected
                            // token (aud/binding/expiry mismatch) is a real
                            // bearer credential, and a garbage token is
                            // attacker-controlled bytes — neither belongs
                            // there. A fixed sentinel is enough for ACF to
                            // deny; the reason is in the warn log.
                            tracing::warn!(peer = %state.peer, error = %e,
                                "cap-token verification failed");
                            "unverified".to_string()
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
                    // C `read_action` (camessage.c:608-610):
                    // `if (!pciu) { logBadId; return RSRV_ERROR; }` —
                    // silent disconnect, no wire reply. Matches the
                    // EVENT_ADD silent-disconnect pattern (round-16
                    // 9fdbc37) where C's logBadId path is "log
                    // server-side, drop the connection". Pre-fix
                    // Rust sent ECA_BADCHID for READ_NOTIFY and
                    // silently kept the connection for READ; both
                    // diverged from C's silent + drop.
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "READ on unknown SID {} (matches C read_action logBadId + RSRV_ERROR)",
                        sid
                    )));
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
                        // R2-7: C `read_notify_action` →
                        // `read_reply` → `no_read_access_event`
                        // (`rsrv/camessage.c:450-480`) builds a
                        // CA_PROTO_READ_NOTIFY frame with the
                        // ORIGINAL requested count and a
                        // `dbr_size_n`-sized zero payload, abusing
                        // `m_cid` to carry the ECA status. Pre-fix
                        // Rust used `send_cmd_error` which always
                        // emits `count = 0` + zero-byte payload — a
                        // libca-style client validating callback
                        // metadata saw the wrong shape for the same
                        // no-read-access `caget` path. The helper
                        // mirrors the C wire format.
                        send_no_read_access_event(
                            writer,
                            CA_PROTO_READ_NOTIFY,
                            requested_type,
                            requested_count,
                            ioid,
                            denied.eca_code(),
                        )
                        .await?;
                    } else {
                        // C `read_action` (`rsrv/camessage.c:636-642`)
                        // sends `send_err(mp, ECA_NORDACCESS, client,
                        // RECORD_NAME(pciu->dbch))` — i.e.
                        // CA_PROTO_ERROR — for the deprecated
                        // CA_PROTO_READ on read denial. Pre-fix Rust
                        // silently returned, so a libca client saw a
                        // timeout instead of the C error callback.
                        // R2-15: outer cid is `pciu->cid` per
                        // `vsend_err` (camessage.c:160-170).
                        let audit_pv = match &entry.target {
                            ChannelTarget::SimplePv(pv) => pv.name.clone(),
                            ChannelTarget::RecordField { record, field } => {
                                format!("{}.{}", record.read().await.name, field)
                            }
                        };
                        send_ca_error(writer, hdr, denied.eca_code(), entry.cid, &audit_pv).await?;
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
                    //
                    // R2-6: C `read_notify_action`
                    // (`rsrv/camessage.c:693-697`) returns
                    // `RSRV_ERROR` on `INVALID_DB_REQ` WITHOUT
                    // emitting any wire frame — only the deprecated
                    // `read_action` (camessage.c:616-620) calls
                    // `send_err(ECA_BADTYPE)` here. Pre-fix Rust
                    // sent a CA_PROTO_READ_NOTIFY error frame for
                    // the notify path too, an extra wire frame
                    // before EOF that rsrv never produces. Mirror C:
                    // notify path is silent; only the deprecated
                    // READ path emits CA_PROTO_ERROR.
                    // R2-15: outer cid is `pciu->cid`.
                    if !is_notify {
                        send_ca_error(writer, hdr, ECA_BADTYPE, entry.cid, "bad READ data type")
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
            //
            // R2-13: C `read_reply` (`rsrv/camessage.c:507-571`) keeps
            // the request count in the header and zero-fills the
            // payload when fewer elements are returned than requested
            // (`autosize = mp->m_count == 0` is the exception:
            // request count 0 means "all available"; otherwise the
            // response carries the requested count and pads with
            // zeros). Pre-fix Rust dropped the requested count on
            // a short array, so a `ca_array_get_callback(type,
            // count > native, ...)` saw a shorter response from
            // Rust than from rsrv.
            let mut data = data;
            let actual_count = snapshot.value.count() as u32;
            let element_count = if requested_type == epics_base_rs::types::DBR_CLASS_NAME {
                1
            } else {
                pad_dbr_to_requested_count(&mut data, actual_count, requested_count, requested_type)
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

            // Abort-safety: a `send_timeout` cancel landing between a
            // separate header and payload `write_all` would leave an
            // orphan header in the shared BufWriter and mis-frame every
            // following message. Build the whole READ/READ_NOTIFY frame
            // as ONE contiguous buffer and issue a single `write_all`,
            // so a cancel can only land at a frame boundary. Same fix
            // already applied to the monitor path (`monitor.rs`).
            let hdr_bytes = resp.to_bytes_extended();
            let mut frame = Vec::with_capacity(hdr_bytes.len() + padded.len());
            frame.extend_from_slice(&hdr_bytes);
            frame.extend_from_slice(&padded);
            let mut w = writer.lock().await;
            w.write_all(&frame).await?;
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
                        // C `write_action` (camessage.c:736-738) +
                        // `write_notify_action` (camessage.c:1642-1645):
                        // `if (!pciu) { logBadId; return RSRV_ERROR; }`
                        // — silent disconnect on missing channel. Same
                        // family as round-16 EVENT_ADD bad-SID and the
                        // matching READ branch below.
                        return Err(epics_base_rs::error::CaError::Protocol(format!(
                            "WRITE (ACKT/ACKS) on unknown SID {} \
                             (matches C write_action logBadId + RSRV_ERROR)",
                            sid
                        )));
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
                let entry_cid = entry.cid;
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
                        } else {
                            // C `write_action` (`rsrv/camessage.c:741-751`)
                            // sends `send_err(mp, ECA_NOWTACCESS, client,
                            // RECORD_NAME(pciu->dbch))` — i.e.
                            // CA_PROTO_ERROR — for the deprecated
                            // CA_PROTO_WRITE on write denial. DBR_PUT_ACKT/
                            // DBR_PUT_ACKS travel the same WRITE opcodes,
                            // so this branch covers alarm-acknowledge PUTs
                            // too. Pre-fix Rust silently dropped the
                            // denied PROTO_WRITE, so a libca client looked
                            // like its put had succeeded — no error
                            // callback even though the value never
                            // reached the DB.
                            // R2-15: outer cid is `pciu->cid` per C
                            // `vsend_err` (camessage.c:160-170), not
                            // the SID we received in `hdr.cid`.
                            let audit_pv = match &entry.target {
                                ChannelTarget::SimplePv(pv) => pv.name.clone(),
                                ChannelTarget::RecordField { record, field } => {
                                    format!("{}.{}", record.read().await.name, field)
                                }
                            };
                            send_ca_error(writer, hdr, denied.eca_code(), entry_cid, &audit_pv)
                                .await?;
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
                } else if let Err(e) = &result {
                    // R2-14: deprecated CA_PROTO_WRITE for DBR_PUT_ACKT/
                    // DBR_PUT_ACKS must surface put failure via
                    // CA_PROTO_ERROR per C `write_action`
                    // (`rsrv/camessage.c:781-789`). Pre-fix the
                    // non-notify alarm-ack path silently swallowed
                    // record-side write errors so the libca peer never
                    // saw the failure.
                    let audit_pv = match &entry.target {
                        ChannelTarget::SimplePv(pv) => pv.name.clone(),
                        ChannelTarget::RecordField { record, field } => {
                            format!("{}.{}", record.read().await.name, field)
                        }
                    };
                    let eca = e.to_eca_status();
                    send_ca_error(writer, hdr, eca, entry_cid, &audit_pv).await?;
                }
                return Ok(());
            }

            // R2-16: C `write_action` (`rsrv/camessage.c:735-739`) and
            // `write_notify_action` (`camessage.c:1641-1645`) call
            // `MPTOPCIU(mp)` BEFORE any DBR-type check, so a bad SID
            // path goes through `logBadId` + RSRV_ERROR (silent drop)
            // regardless of whether the type is also invalid. Pre-fix
            // Rust ran the type check first and emitted an ECA_BADTYPE
            // error frame for the SID+type combo where rsrv would
            // have closed silently. Reorder to match C.
            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    // Same C logBadId + RSRV_ERROR family as the
                    // ACKT/ACKS branch above and the READ branch.
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "WRITE on unknown SID {} (matches C write_action logBadId + RSRV_ERROR)",
                        sid
                    )));
                }
            };
            // R2-15: channel-scoped CA_PROTO_ERROR replies must echo
            // `pciu->cid` (the CLIENT cid the libca peer allocated),
            // not the server-side SID we received in `hdr.cid`. C
            // `vsend_err` (`rsrv/camessage.c:160-170`) looks up the
            // `channel_in_use` and uses its `cid` field for the outer
            // error header. Captured here as a Copy so the error sites
            // below can use it after the `entry` borrow ends.
            let entry_cid = entry.cid;

            // Resolve the audit-friendly PV name once. Cheap when audit
            // is off because state.audit() is a single None check.
            let audit_pv = match &entry.target {
                ChannelTarget::SimplePv(pv) => pv.name.clone(),
                ChannelTarget::RecordField { record, field } => {
                    format!("{}.{}", record.read().await.name, field)
                }
            };

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
                        send_ca_error(writer, hdr, ECA_BADTYPE, entry_cid, "bad data type").await?;
                    }
                    return Err(epics_base_rs::error::CaError::Protocol(format!(
                        "WRITE with unsupported DBR type {} (matches C write_action RSRV_ERROR)",
                        hdr.data_type
                    )));
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
                        send_ca_error(writer, hdr, denied.eca_code(), entry_cid, &audit_pv).await?;
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
                        send_ca_error(
                            writer,
                            hdr,
                            ECA_BADTYPE,
                            entry_cid,
                            "bad WRITE payload bytes",
                        )
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

            // R2-14: C `write_action` (`rsrv/camessage.c:781-789`):
            // even the deprecated fire-and-forget `CA_PROTO_WRITE`
            // surfaces a failed `dbChannel_put` to the client via
            // `send_err(mp, ECA_PUTFAIL, ...)`. Pre-fix Rust dropped
            // the failure silently for the non-notify path, so a
            // `caput` against a read-only-by-rule field that bypassed
            // earlier access checks (e.g. record-side `PutDisabled`)
            // looked successful to the libca peer even though the
            // value never reached the DB. is_notify already replies
            // via WRITE_NOTIFY below.
            if !is_notify {
                if let Err(e) = &write_result {
                    let eca = e.to_eca_status();
                    send_ca_error(writer, hdr, eca, entry_cid, &audit_pv).await?;
                }
            }

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

                        let denied = Arc::new(AtomicBool::new(false));
                        let task = spawn_monitor_sender(
                            pv.clone(),
                            sub_id,
                            requested_type,
                            writer.clone(),
                            state.flow_control.clone(),
                            rx,
                            denied.clone(),
                        );

                        state.subscriptions.insert(
                            sub_id,
                            SubscriptionEntry {
                                target: ChannelTarget::SimplePv(pv.clone()),
                                channel_sid: sid,
                                sub_id,
                                data_type: requested_type,
                                denied,
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
                        let denied = Arc::new(AtomicBool::new(false));
                        let denied_for_task = denied.clone();
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
                                // C `casAccessRightsCB`
                                // (`rsrv/camessage.c:1080-1095`)
                                // suppresses delivery via
                                // `db_event_disable` while read access
                                // is denied, without tearing the
                                // subscription down. Producer task
                                // stays alive so a later re-enable
                                // resumes the same camonitor; drop
                                // the event here while denied.
                                if denied_for_task.load(Ordering::Acquire) {
                                    continue;
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

                                // Abort-safety: this monitor task can
                                // be `task.abort()`ed mid-flight by
                                // EVENT_CANCEL / CLEAR_CHANNEL /
                                // disconnect cleanup. Build the whole
                                // EVENT_ADD frame (header + padded
                                // payload) as ONE contiguous buffer and
                                // issue a single `write_all`, so an
                                // abort can only land at a frame
                                // boundary, never between header and
                                // payload — a split there would leave
                                // an orphan header in the shared
                                // BufWriter and mis-frame the stream.
                                let hdr_bytes = hdr.to_bytes_extended();
                                let mut frame = Vec::with_capacity(hdr_bytes.len() + padded.len());
                                frame.extend_from_slice(&hdr_bytes);
                                frame.extend_from_slice(&padded);
                                let mut w = writer_clone.lock().await;
                                if w.write_all(&frame).await.is_err() {
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
                                denied,
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
            let req_channel_sid = hdr.cid;
            // C `event_cancel_reply` (camessage.c:2002-2010) walks
            // the CHANNEL's eventq looking for a matching sub-id.
            // The cross-check is implicit: a sub-id that exists but
            // belongs to a different channel is "not found on this
            // channel" and falls through to the ECA_BADMONID +
            // RSRV_ERROR path. Rust's `state.subscriptions` is a
            // flat HashMap by sub-id; we have to add the
            // cross-check explicitly. If we skipped it, a peer
            // could send EVENT_CANCEL with wrong cid but valid
            // sub-id and erase a real subscription bound to a
            // different channel — bypass of round-21's BAD-MONID
            // disconnect.
            let channel_matches = state
                .subscriptions
                .get(&sub_id)
                .is_some_and(|s| s.channel_sid == req_channel_sid);
            if !channel_matches {
                // Trigger the round-21 BAD-MONID path: emit
                // ECA_BADMONID + disconnect. Use the request's
                // m_cid for the diag PV name when it resolves.
                let (chan_cid, diag) = match state.channels.get(&req_channel_sid) {
                    Some(entry) => (entry.cid, entry.pv_name.clone()),
                    None => (0xFFFF_FFFFu32, "unknown".to_string()),
                };
                tracing::debug!(
                    sub_id,
                    sid = req_channel_sid,
                    "EVENT_CANCEL channel-mismatch (sub belongs to different channel); ECA_BADMONID"
                );
                send_ca_error(writer, hdr, ECA_BADMONID, chan_cid, &diag).await?;
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "EVENT_CANCEL sub-id {} channel-mismatch (requested sid {}; \
                     matches C event_cancel_reply 'not on this channel's eventq' RSRV_ERROR)",
                    sub_id, req_channel_sid
                )));
            }
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
                // C `event_cancel_reply` (camessage.c:2016-2021):
                // after `send_err(ECA_BADMONID)`, return RSRV_ERROR
                // which tears the connection down. Pre-fix Rust kept
                // the connection; a peer racing CLEAR_CHANNEL against
                // EVENT_CANCEL on the same sub-id could spam the
                // server with stale cancels indefinitely.
                return Err(epics_base_rs::error::CaError::Protocol(format!(
                    "EVENT_CANCEL for unknown sub-id {} \
                     (matches C event_cancel_reply ECA_BADMONID + RSRV_ERROR)",
                    sub_id
                )));
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
            // Abort-safety: build header + echoed payload as ONE
            // contiguous frame and issue a single `write_all`, so a
            // `send_timeout` cancel cannot leave an orphan header
            // mid-frame in the shared BufWriter.
            let mut frame = Vec::new();
            if resp.is_extended() {
                frame.extend_from_slice(&resp.to_bytes_extended());
            } else {
                frame.extend_from_slice(&resp.to_bytes());
            }
            // Echo the payload back verbatim (truncated to the actual
            // postsize advertised by the request — `payload` here is
            // already that slice).
            frame.extend_from_slice(payload);
            let mut w = writer.lock().await;
            w.write_all(&frame).await?;
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

    // Abort-safety: build header + payload as ONE contiguous frame and
    // issue a single `write_all` so a cancel (send_timeout / task abort)
    // cannot leave an orphan header mid-frame in the shared BufWriter.
    let hdr_bytes = resp.to_bytes_extended();
    let mut frame = Vec::with_capacity(hdr_bytes.len() + padded.len());
    frame.extend_from_slice(&hdr_bytes);
    frame.extend_from_slice(&padded);
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Re-evaluate and re-send CA_PROTO_ACCESS_RIGHTS for all open channels.
/// Called when hostname or username changes (e.g. ACF reload).
///
/// Tracks the *transition* per channel because the C behaviour is
/// asymmetric: a read-access loss must push a single
/// `no_read_access_event` frame and silence subsequent deliveries,
/// while a read-access gain must re-enable deliveries and push one
/// current snapshot. C `casAccessRightsCB`
/// (`rsrv/camessage.c:1055-1106`) walks the channel's `eventq` and
/// calls `db_event_disable` / `db_event_enable` plus
/// `db_post_single_event` — the subscription itself is never
/// removed. Pre-fix Rust permanently destroyed the subscription
/// on a NoAccess transition (`state.subscriptions.remove +
/// task.abort`), so a later ACF reload that restored read access
/// left an orphaned camonitor: the C-equivalent re-arm never
/// happened, and the subscriber's callback receiver went silent
/// until the client noticed and re-subscribed manually.
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

    // (sid, old_level, new_level) — old defaults to NoAccess for a
    // sid the access cache has not seen before (parity with the
    // pre-fix `insert`-without-comparison behaviour for freshly
    // created channels).
    let mut transitions: Vec<(u32, AccessLevel, AccessLevel)> = Vec::new();
    {
        let mut w = writer.lock().await;
        for (sid, cid, target) in chan_info {
            let new_access = state.compute_access(&target).await;
            let new_level = match new_access {
                3 => AccessLevel::ReadWrite,
                1 => AccessLevel::Read,
                _ => AccessLevel::NoAccess,
            };
            let old_level = state
                .channel_access
                .insert(sid, new_level)
                .unwrap_or(AccessLevel::NoAccess);
            transitions.push((sid, old_level, new_level));
            let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
            ar.cid = cid;
            ar.available = new_access;
            w.write_all(&ar.to_bytes()).await?;
        }
        w.flush().await?;
    }

    fn has_read(level: AccessLevel) -> bool {
        matches!(level, AccessLevel::ReadWrite | AccessLevel::Read)
    }

    for (sid, old_level, new_level) in transitions {
        let old_read = has_read(old_level);
        let new_read = has_read(new_level);
        if old_read == new_read {
            continue;
        }
        let affected: Vec<u32> = state
            .subscriptions
            .iter()
            .filter(|(_, s)| s.channel_sid == sid)
            .map(|(&id, _)| id)
            .collect();
        if affected.is_empty() {
            continue;
        }
        if !new_read {
            // Read access REVOKED. C path: db_post_single_event
            // (which emits the `no_read_access_event` —
            // ECA_NORDACCESS in m_cid, zeroed payload) then
            // db_event_disable. We flip the gate so the producer
            // task suppresses future deliveries, and emit a
            // header-only EVENT_ADD frame carrying ECA_NORDACCESS
            // in m_cid; libca's `cac::eventAddRespAction`
            // (`cac.cpp:973-977`) routes that through the
            // per-subscription exception callback without touching
            // the payload bytes (it never reads them on the
            // non-NORMAL path), so a header-only frame is
            // sufficient for modern peers and matches our own
            // client's MonitorStatusError path. The subscription
            // itself stays alive so a later access restoration can
            // re-arm it.
            for sub_id in &affected {
                let Some(sub) = state.subscriptions.get(sub_id) else {
                    continue;
                };
                sub.denied.store(true, Ordering::Release);
                let data_type = sub.data_type;
                let sub_id_v = sub.sub_id;
                let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
                hdr.data_type = data_type;
                hdr.cid = ECA_NORDACCESS;
                hdr.available = sub_id_v;
                let mut w = writer.lock().await;
                w.write_all(&hdr.to_bytes()).await?;
            }
            let mut w = writer.lock().await;
            w.flush().await?;
        } else {
            // Read access RESTORED. C path: db_event_enable then
            // db_post_single_event. Clear the gate so the producer
            // task resumes deliveries, and emit one snapshot of
            // the current value so the subscriber sees a fresh
            // event the moment access comes back (rather than
            // waiting for the next natural update).
            for sub_id in &affected {
                let (target, data_type, sub_id_val) = {
                    let Some(sub) = state.subscriptions.get(sub_id) else {
                        continue;
                    };
                    sub.denied.store(false, Ordering::Release);
                    (sub.target.clone(), sub.data_type, sub.sub_id)
                };
                if let Some(snap) = get_full_snapshot(&target).await {
                    send_monitor_snapshot(writer, sub_id_val, data_type, &snap).await?;
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

/// Send a `no_read_access_event`-shaped reply: same wire frame as the
/// original READ_NOTIFY / EVENT_ADD command, with `m_cid` carrying the
/// ECA status and a `dbr_buffer_size`-sized zero payload. C
/// `no_read_access_event` (`rsrv/camessage.c:450-480`) and `read_reply`
/// (`camessage.c:540-557`) use this shape for READ_NOTIFY denials and
/// dbChannel_get failures — preserving the requested count and DBR
/// type so libca-style clients see the correct callback metadata even
/// on the error path.
async fn send_no_read_access_event<W: AsyncWrite + Unpin + Send + 'static>(
    writer: &Arc<Mutex<BufWriter<W>>>,
    cmd: u16,
    data_type: u16,
    count: u32,
    available: u32,
    eca_status: u32,
) -> CaResult<()> {
    let native = epics_base_rs::types::native_type_for_dbr(data_type)
        .unwrap_or(epics_base_rs::types::DbFieldType::Char);
    let payload_size = epics_base_rs::types::dbr_buffer_size(data_type, native, count as usize);
    let padded_size = align8(payload_size);
    let mut hdr = CaHeader::new(cmd);
    hdr.set_payload_size(padded_size, count);
    hdr.data_type = data_type;
    hdr.cid = eca_status;
    hdr.available = available;
    let hdr_bytes = hdr.to_bytes_extended();
    // Build header + zero payload as one contiguous frame so a
    // task abort can only land at a frame boundary (same abort-
    // safety invariant as `send_event` / `send_monitor_snapshot`).
    let mut frame = Vec::with_capacity(hdr_bytes.len() + padded_size);
    frame.extend_from_slice(&hdr_bytes);
    frame.resize(frame.len() + padded_size, 0);
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    Ok(())
}

/// Zero-pad an encoded DBR payload up to the requested element count.
/// C `read_reply` (`rsrv/camessage.c:566-571`) keeps the request-count
/// header and pads bytes past the actual element count with zeros.
/// Returns the header element count to use (`requested_count` when
/// non-zero, `actual_count` when zero / autosize).
fn pad_dbr_to_requested_count(
    encoded: &mut Vec<u8>,
    actual_count: u32,
    requested_count: u32,
    data_type: u16,
) -> u32 {
    if requested_count == 0 {
        return actual_count;
    }
    if requested_count > actual_count {
        if let Ok(native) = epics_base_rs::types::native_type_for_dbr(data_type) {
            let extra = (requested_count - actual_count) as usize * native.element_size();
            encoded.extend(std::iter::repeat_n(0u8, extra));
        }
    }
    requested_count
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

    // Abort-safety: a CA_PROTO_ERROR reply is response-header +
    // echoed-request-header + diagnostic string. Build all three as ONE
    // contiguous frame and issue a single `write_all` so a `send_timeout`
    // cancel cannot leave a partial frame (orphan header) in the shared
    // BufWriter and mis-frame every following message.
    //
    // The echoed request header is emitted in extended form when the
    // original request used the extended layout. C `vsend_err`
    // (`rsrv/camessage.c:201-214`) writes a 16-byte header with
    // `m_postsize = 0xffff` plus an 8-byte annex carrying the full
    // 32-bit postsize / count; libca `cac::exceptionRespAction`
    // (`modules/ca/src/client/cac.cpp:1097-1107`) parses the annex
    // first when it sees the 0xffff marker, then walks the diag
    // string from the post-annex offset. `to_bytes_extended()`
    // produces exactly that layout (24 bytes when `is_extended()`,
    // 16 bytes otherwise), so an extended READ/WRITE error
    // round-trips byte-for-byte with libca.
    let resp_bytes = resp.to_bytes_extended();
    let orig_bytes = original_hdr.to_bytes_extended();
    let mut frame = Vec::with_capacity(resp_bytes.len() + orig_bytes.len() + error_msg_bytes.len());
    frame.extend_from_slice(&resp_bytes);
    frame.extend_from_slice(&orig_bytes);
    frame.extend_from_slice(&error_msg_bytes);
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
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

#[cfg(test)]
mod extended_header_split_tests {
    //! C-parity regression: a TCP segment that ends in the middle of an
    //! extended-form header (16..24 bytes, `m_postsize == 0xffff`) must
    //! make the framing loop *wait* for the rest of the header, not
    //! disconnect the client. C `rsrv/camessage.c:~2410` does
    //! `status = RSRV_OK; break;` for this partial-header case.
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Feed exactly 20 bytes of an extended-form header (a 16-byte base
    /// header with `postsize == 0xFFFF`, plus 4 of the 8 extended
    /// bytes) and assert `handle_client` does NOT return early with an
    /// error: it must block awaiting the remaining 4 bytes. Pre-fix,
    /// `from_bytes_extended` returned `Err("extended header
    /// incomplete")` and the `?` closed the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_extended_header_waits_not_disconnects() {
        let db = Arc::new(PvDatabase::new());
        let acf = Arc::new(tokio::sync::RwLock::new(None));
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);

        let (client_io, server_io) = tokio::io::duplex(256);
        let peer: SocketAddr = "127.0.0.1:55123".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                None,
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        // Build a CA_PROTO_READ_NOTIFY header in extended form:
        // postsize=0xFFFF marks extended; write only 20 of the 24
        // header bytes so the framing loop sees a partial ext header.
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.postsize = 0xFFFF;
        let base = hdr.to_bytes();
        let mut prefix = base.to_vec();
        // 4 of the 8 extended bytes (extended postsize = 0).
        prefix.extend_from_slice(&[0u8, 0, 0, 0]);
        assert_eq!(prefix.len(), 20);

        let mut client = client_io;
        client.write_all(&prefix).await.expect("write prefix");
        client.flush().await.expect("flush prefix");

        // The handler must still be running — it is waiting for the
        // remaining 4 bytes, not disconnected with an error.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "handle_client returned on a partial extended header — \
             must wait for more bytes (C camessage.c RSRV_OK; break)"
        );

        // Close the write half: a clean EOF on a partial frame must
        // resolve to Ok(()), never an Err.
        drop(client);
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes after EOF")
            .expect("join ok");
        assert!(
            res.is_ok(),
            "clean EOF after partial extended header must be Ok, got {res:?}"
        );
    }
}

#[cfg(test)]
mod non_graceful_disconnect_teardown_tests {
    //! CRITICAL regression: every exit path out of `handle_client`'s
    //! read loop — not just the graceful `break` — must run the single
    //! teardown block (subscription cancel + `SubscriptionClosed`
    //! emission + write-notify abort + `ChannelCleared` emission).
    //!
    //! Before the fix, an in-loop `return Err(..)` (misaligned payload,
    //! payload-too-large, dispatch error, send-timeout, rate-limit
    //! disconnect, batched-flush error) bypassed the teardown. A client
    //! that established a subscription and then disconnected
    //! non-gracefully (TCP RST, malformed frame) would leave its
    //! `SubscriptionClosed` event unfired forever — inflating consumer
    //! refcounts that key off these events (e.g. ca_gateway).
    use super::*;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::types::EpicsValue;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Build a CA_PROTO_VERSION request (minor version 13).
    fn version_frame() -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.to_bytes().to_vec()
    }

    /// Build a CA_PROTO_CREATE_CHAN request for `pv_name` with the
    /// given client cid. Payload is the 8-aligned, NUL-terminated name.
    fn create_chan_frame(cid: u32, pv_name: &str) -> Vec<u8> {
        let name = pad_string(pv_name);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        h.set_payload_size(name.len(), 0);
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&name);
        frame
    }

    /// Build a CA_PROTO_EVENT_ADD request: subscribe `sub_id` on `sid`.
    /// Payload is the 16-byte monitor request (low/high/to f32 + mask).
    fn event_add_frame(sid: u32, sub_id: u32) -> Vec<u8> {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = epics_base_rs::types::DBR_TIME_DOUBLE;
        h.count = 1;
        h.cid = sid;
        h.available = sub_id;
        h.set_payload_size(16, 1);
        let mut frame = h.to_bytes().to_vec();
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&0f32.to_be_bytes());
        frame.extend_from_slice(&3u16.to_be_bytes()); // mask: value+alarm
        frame.extend_from_slice(&0u16.to_be_bytes()); // pad
        frame
    }

    /// Drain `rx` for up to `timeout`, returning the first event that
    /// satisfies `pred`, or `None` on timeout.
    async fn await_event(
        rx: &mut broadcast::Receiver<ServerConnectionEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&ServerConnectionEvent) -> bool,
    ) -> Option<ServerConnectionEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if pred(&ev) {
                        return Some(ev);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }

    /// Read from `client` until a CA_PROTO_CREATE_CHAN response frame is
    /// seen, then return its server-assigned sid (`m_available`). The
    /// EVENT_ADD request must address the channel by this sid, not by
    /// the client cid.
    async fn read_create_chan_sid<R: tokio::io::AsyncRead + Unpin>(
        client: &mut R,
        timeout: Duration,
    ) -> u32 {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for CREATE_CHAN response"
            );
            let n = tokio::time::timeout(remaining, client.read(&mut buf))
                .await
                .expect("read within timeout")
                .expect("read ok");
            assert!(n > 0, "server closed before CREATE_CHAN response");
            acc.extend_from_slice(&buf[..n]);
            let mut offset = 0;
            while offset + CaHeader::SIZE <= acc.len() {
                let (hdr, hdr_size) = CaHeader::from_bytes_extended(&acc[offset..])
                    .expect("server response header parses");
                let msg_len = hdr_size + hdr.actual_postsize();
                if offset + msg_len > acc.len() {
                    break;
                }
                if hdr.cmmd == CA_PROTO_CREATE_CHAN {
                    return hdr.available;
                }
                offset += msg_len;
            }
        }
    }

    /// A client that opens a subscription and then sends a misaligned
    /// frame (postsize not 8-aligned) MUST still have its
    /// `SubscriptionClosed` event emitted. The misaligned frame drives
    /// the `break 'client_loop Err(..)` path that previously bypassed
    /// the teardown block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn misaligned_frame_after_subscribe_still_emits_subscription_closed() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("teardown:test:pv", EpicsValue::Double(1.0))
            .await
            .expect("add pv");
        let acf = Arc::new(tokio::sync::RwLock::new(None));
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, mut conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55222".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        // Establish the channel first; read back the server-assigned sid.
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xAA, "teardown:test:pv"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;
        // Now subscribe, addressing the channel by its server sid.
        client
            .write_all(&event_add_frame(sid, 0xBB))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        // The server must accept the subscription before we test the
        // teardown — otherwise the test would pass vacuously.
        let opened = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionOpened { .. })
        })
        .await;
        assert!(
            matches!(opened, Some(ServerConnectionEvent::SubscriptionOpened { sub_id, .. }) if sub_id == 0xBB),
            "subscription must open before the disconnect test (got {opened:?})"
        );

        // Now send a definitively malformed frame: a header whose
        // postsize is not 8-byte aligned. The server rejects it with
        // ECA_INTERNAL and `break 'client_loop Err(..)` — a NON-graceful
        // exit. Before the fix this `return Err` bypassed the teardown.
        let mut bad = CaHeader::new(CA_PROTO_READ_NOTIFY);
        bad.postsize = 5; // not a multiple of 8 — misaligned
        client
            .write_all(&bad.to_bytes())
            .await
            .expect("misaligned frame");
        client.flush().await.expect("flush misaligned");

        // The teardown MUST emit SubscriptionClosed for sub_id 0xBB even
        // though the connection ended via the error path.
        let closed = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionClosed { .. })
        })
        .await;
        assert!(
            matches!(closed, Some(ServerConnectionEvent::SubscriptionClosed { sub_id, .. }) if sub_id == 0xBB),
            "SubscriptionClosed must fire on a non-graceful (error-path) \
             disconnect — teardown was bypassed (got {closed:?})"
        );

        // The handler returns Err for the misaligned-frame path.
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes")
            .expect("join ok");
        assert!(
            res.is_err(),
            "misaligned frame must close the connection with Err, got {res:?}"
        );
        drop(client);
    }

    /// Control case: a graceful EOF disconnect must ALSO emit
    /// `SubscriptionClosed` (the path that always worked) — guards
    /// against the restructure regressing the `break` path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_eof_after_subscribe_emits_subscription_closed() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("teardown:test:pv2", EpicsValue::Double(1.0))
            .await
            .expect("add pv");
        let acf = Arc::new(tokio::sync::RwLock::new(None));
        let (_acf_reload_tx, acf_reload_rx) = broadcast::channel::<()>(4);
        let (conn_tx, mut conn_rx) = broadcast::channel::<ServerConnectionEvent>(64);

        let (client_io, server_io) = tokio::io::duplex(4096);
        let peer: SocketAddr = "127.0.0.1:55223".parse().unwrap();

        let handle = tokio::spawn(async move {
            handle_client(
                server_io,
                peer,
                db,
                acf,
                acf_reload_rx,
                5064,
                None,
                None,
                None,
                Some(conn_tx),
                None,
                #[cfg(feature = "cap-tokens")]
                None,
                #[cfg(feature = "cap-tokens")]
                None,
            )
            .await
        });

        let mut client = client_io;
        client.write_all(&version_frame()).await.expect("version");
        client
            .write_all(&create_chan_frame(0xCC, "teardown:test:pv2"))
            .await
            .expect("create_chan");
        client.flush().await.expect("flush create_chan");
        let sid = read_create_chan_sid(&mut client, Duration::from_secs(3)).await;
        client
            .write_all(&event_add_frame(sid, 0xDD))
            .await
            .expect("event_add");
        client.flush().await.expect("flush event_add");

        let opened = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionOpened { .. })
        })
        .await;
        assert!(opened.is_some(), "subscription must open");

        // Graceful close: drop the write half → server reads EOF →
        // `break 'client_loop Ok(())`.
        drop(client);

        let closed = await_event(&mut conn_rx, Duration::from_secs(3), |ev| {
            matches!(ev, ServerConnectionEvent::SubscriptionClosed { .. })
        })
        .await;
        assert!(
            matches!(closed, Some(ServerConnectionEvent::SubscriptionClosed { sub_id, .. }) if sub_id == 0xDD),
            "SubscriptionClosed must fire on graceful EOF too (got {closed:?})"
        );

        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_client completes")
            .expect("join ok");
        assert!(res.is_ok(), "graceful EOF must be Ok, got {res:?}");
    }
}

#[cfg(test)]
mod single_write_all_framing_tests {
    //! BUG 4: GET/READ_NOTIFY, introspection (`send_monitor_snapshot`)
    //! and CA_PROTO_ERROR (`send_ca_error`) replies must be written to
    //! the shared `BufWriter` as ONE contiguous `write_all`. A split
    //! across two `write_all` awaits lets a `send_timeout` cancel land
    //! between header and payload, leaving an orphan header that
    //! mis-frames every following message. A true cancel-race is
    //! non-deterministic; this asserts the structural property that
    //! makes the race impossible: exactly one write batch == one frame.
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    /// Mock `AsyncWrite` recording each `poll_write` batch. Wrapped in a
    /// zero-capacity `BufWriter`, batch count == `write_all` count.
    #[derive(Default)]
    struct RecordingWriter {
        batches: Vec<Vec<u8>>,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.batches.push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn recording_writer() -> Arc<Mutex<BufWriter<RecordingWriter>>> {
        // Zero capacity: every write_all forwards straight through.
        Arc::new(Mutex::new(BufWriter::with_capacity(
            0,
            RecordingWriter::default(),
        )))
    }

    /// `send_ca_error` builds response-header + echoed-request-header +
    /// diagnostic string. All three must leave in a single `write_all`.
    #[tokio::test]
    async fn send_ca_error_writes_single_frame() {
        let writer = recording_writer();
        let original = CaHeader::new(CA_PROTO_READ_NOTIFY);

        send_ca_error(
            &writer,
            &original,
            ECA_INTERNAL,
            0xFFFF_FFFF,
            "CAS: Missaligned protocol rejected",
        )
        .await
        .expect("send_ca_error succeeds");

        let guard = writer.lock().await;
        let batches = &guard.get_ref().batches;
        assert_eq!(
            batches.len(),
            1,
            "send_ca_error must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );
        // The one batch must be the complete frame: response header +
        // 16-byte echoed request header + padded diagnostic string.
        let frame = &batches[0];
        let payload_size = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_eq!(
            16 + payload_size,
            frame.len(),
            "CA_PROTO_ERROR header-declared size must match the contiguous frame",
        );
    }

    /// `send_monitor_snapshot` (the introspection EVENT_ADD reply) must
    /// emit header + padded payload as a single `write_all`.
    #[tokio::test]
    async fn send_monitor_snapshot_writes_single_frame() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let writer = recording_writer();
        let snapshot = Snapshot::new(
            EpicsValue::Long(123),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );

        send_monitor_snapshot(&writer, 9, DBR_LONG, &snapshot)
            .await
            .expect("send_monitor_snapshot succeeds");

        let guard = writer.lock().await;
        let batches = &guard.get_ref().batches;
        assert_eq!(
            batches.len(),
            1,
            "send_monitor_snapshot must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );
    }
}
