//! Background search engine.
//!
//! Owns:
//!
//! - A **search socket** (ephemeral UDP) used to broadcast SEARCH requests
//!   and receive SEARCH_RESPONSE messages.
//! - A **beacon socket** (UDP bound to 5076 with `SO_REUSEPORT`) used to
//!   listen for unsolicited server BEACON messages.
//!
//! The engine drives:
//!
//! - Per-PV search retry with pvxs-style backoff (15s → 30s → 60s → 120s
//!   → 210s capped).
//! - Beacon-driven fast reconnect: when a beacon arrives for a server we
//!   have a disconnected channel against, the engine re-issues SEARCH for
//!   that channel immediately.
//! - Beacon anomaly throttling via [`super::beacon_throttle::BeaconTracker`].

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tracing::{debug, warn};

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    ByteOrder, Command, ControlCommand, PvaHeader, ReadExt, WriteExt, decode_size, decode_string,
    encode_string_into, ip_from_bytes_allow_unspec,
};

use super::beacon_throttle::BeaconTracker;
use super::decode::{PeerRole, decode_search_response, try_parse_frame_role};

/// Search retry backoff sequence (seconds), matching pvxs `clientdiscover.cpp`.
pub const BACKOFF_SECS: &[u64] = &[1, 1, 2, 5, 10, 15, 30, 60, 120, 210];

/// Default UDP broadcast port for SEARCH/BEACON messages (5076).
pub const DEFAULT_BROADCAST_PORT: u16 = 5076;

/// PR #205 IPv6 Stage 4: default v6 multicast group for PVA SEARCH.
/// pvxs `udp_collector.cpp` uses `ff0e::400` (organization-local,
/// dynamic) for the IPv6 equivalent of the v4 `224.0.2.3` group.
/// Joined/sent only when an IPv6 send socket is available.
pub const DEFAULT_V6_MULTICAST_GROUP: Ipv6Addr = Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x0400);

/// Command sent into the engine.
/// Why a search is being initiated — controls bucket placement and
/// whether the first SEARCH packet fires immediately.
///
/// Mirrors pvxs `client.cpp` (Channel::disconnect / tickSearch):
///
/// - `Initial` is a fresh resolve: the engine fires an immediate
///   broadcast AND places the search at `current_bucket+1` so the
///   first scheduled retransmit lands one tick after the
///   immediate fire.
///
/// - `Reconnect` follows pvxs `Channel::disconnect` (client.cpp:213)
///   semantics: the search is placed at `current_bucket` with no
///   immediate broadcast, and the next 1 Hz tick fires it. The
///   per-channel retry escalation (`tickSearch` line 1193:
///   `nSearch+1` bucket forward push, capped at `nBuckets`) handles
///   subsequent retransmits at 1 s, 2 s, 3 s, ..., 30 s. Cascade-
///   spread for mass-disconnects is achieved by the natural
///   one-bucket-per-tick rate-limit plus a smoothing rule that
///   defers to `next+1` when the chosen retry bucket is overloaded
///   by 100+ entries (`tickSearch` line 1199).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchReason {
    /// Brand-new resolve; channel has never been Active.
    Initial,
    /// Channel was Active and the underlying ServerConn died (or the
    /// server sent CMD_DESTROY_CHANNEL). pvxs-equivalent recovery
    /// path — see the type-level comment.
    Reconnect,
}

pub enum SearchCommand {
    /// Resolve `pv_name` → first server address. Reply via `responder`
    /// once a SEARCH_RESPONSE comes in.
    Find {
        pv_name: String,
        responder: oneshot::Sender<SocketAddr>,
        reason: SearchReason,
    },
    /// Resolve `pv_name` and collect *all* responses received within
    /// the next [`MULTI_SERVER_WINDOW`]. The reply contains every
    /// server that claimed the PV; the caller can fan-out / fail over.
    FindAll {
        pv_name: String,
        responder: oneshot::Sender<Vec<SocketAddr>>,
        reason: SearchReason,
    },
    /// Cancel an outstanding search (channel was dropped or closed).
    Cancel { pv_name: String },
    /// Notify the engine that we observed a beacon — used by external code
    /// (e.g. when running an embedded server in the same process) to feed
    /// beacons into the throttle without binding the multicast port.
    BeaconObserved { server: SocketAddr, guid: [u8; 12] },
    /// Subscribe to discovery events. The returned receiver yields a
    /// `Discovered` for every beacon that observation logic regards as a
    /// new server (first-seen GUID) or a re-observed-after-restart GUID.
    Subscribe {
        responder: oneshot::Sender<mpsc::Receiver<Discovered>>,
    },
    /// Force the engine into fast-tick mode for one revolution and
    /// bring every pending search's retry deadline forward. Mirrors
    /// pvxs `Context::hurryUp` — same behaviour as a fresh beacon
    /// from a new server, but driven externally (e.g. an app that
    /// learned via OOB channel that an IOC restarted).
    HurryUp,
    /// Drop any cached state for a single PV name: cancel its
    /// outstanding search if one exists. The next `find()` call
    /// starts a fresh search round. Mirrors pvxs `Context::cacheClear`.
    CacheClear { pv_name: String },
    /// Replace the GUID blocklist. Beacons / search responses whose
    /// server GUID matches an entry are silently ignored. Mirrors
    /// pvxs `Context::ignoreServerGUIDs` (client.cpp:453, consulted
    /// at procSearchReply client.cpp:857).
    IgnoreServerGuids { guids: Vec<[u8; 12]> },
    /// Send a "discover" SEARCH (no PV names; flags bit
    /// SEARCH_DISCOVER set) to broadcast targets so any reachable
    /// server replies with a SEARCH_RESPONSE we can convert into
    /// `Discovered::Online`. Mirrors pvxs
    /// `DiscoverBuilder::pingAll(true)` exec path. Effective when the
    /// caller is set up for active discovery rather than passive
    /// beacon listening.
    DiscoverPing,
}

/// Discovery event delivered to subscribers of [`SearchEngine::discover`].
#[derive(Debug, Clone)]
pub enum Discovered {
    /// A beacon arrived for a (server, guid) pair we hadn't seen before,
    /// or a known server reported a different GUID (i.e. restarted).
    ///
    /// `peer` is the UDP source address (origin of the beacon datagram)
    /// while `server` is the advertised TCP endpoint. They differ when
    /// the server binds 0.0.0.0 — the beacon's payload `server` slot is
    /// 0.0.0.0:port and we rewrite it to the peer's IP. `proto` carries
    /// the advertised protocol string ("tcp" / "tls"). pvxs
    /// `Discovered` exposes the same four fields (client.h:967).
    Online {
        server: SocketAddr,
        guid: [u8; 12],
        peer: SocketAddr,
        proto: String,
    },
    /// A server we were tracking has stopped sending beacons for at
    /// least `BEACON_TIMEOUT`. Mirrors pvxs `Discovered::Timeout`
    /// (client.cpp:1272) — operators / dashboards use this to mark
    /// servers as unreachable without waiting for a TCP error.
    Timeout { server: SocketAddr, guid: [u8; 12] },
}

/// Maximum age of a beacon before the server is treated as offline.
/// pvxs uses 2× the beacon-clean interval (default 360s); we match.
pub const BEACON_TIMEOUT: Duration = Duration::from_secs(360);

/// Period of the beacon-cleanup tick. pvxs runs `tickBeaconClean` every
/// 180s; we match.
pub const BEACON_CLEAN_INTERVAL: Duration = Duration::from_secs(180);

/// How long the engine collects extra SEARCH_RESPONSE entries after the
/// first one for a given pv name (used by [`SearchCommand::FindAll`]).
pub const MULTI_SERVER_WINDOW: Duration = Duration::from_millis(200);

/// Public handle to the engine. Cheap to clone (it's just a sender).
#[derive(Clone)]
pub struct SearchEngine {
    cmd_tx: mpsc::Sender<SearchCommand>,
    pub beacons: Arc<BeaconTracker>,
}

impl SearchEngine {
    /// Spawn the engine. Returns a handle that channels use to issue
    /// `find()` requests.
    pub async fn spawn(
        mut extra_targets: Vec<SocketAddr>,
        name_servers: Vec<SocketAddr>,
    ) -> PvaResult<Self> {
        let beacons = BeaconTracker::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SearchCommand>(256);

        let search_socket = bind_ephemeral_udp()?;
        // PR #205 IPv6 Stage 4: optional v6 send/recv socket. Used
        // alongside the v4 socket — graceful degradation when the
        // host has no usable IPv6 stack.
        let search_socket_v6 = bind_ephemeral_udp_v6();
        let beacon_socket = bind_beacon_udp(); // Optional — may be None.
        // PR #205 IPv6 Stage 6: optional v6 beacon listener bound to
        // `[::]:5076` joined to `ff0e::400`. Receives v6 multicast
        // beacons emitted by the server's Stage 5 path. None when the
        // host has no v6.
        let beacon_socket_v6 = bind_beacon_udp_v6();

        // pvxs 8db40be (2025-10): warn loudly when a Context is built
        // with no search destinations and AUTO_ADDR_LIST disabled.
        // The user otherwise sees nothing but timeouts.
        let auto_addr = std::env::var("EPICS_PVA_AUTO_ADDR_LIST").unwrap_or_else(|_| "YES".into());
        let auto_on = matches!(
            auto_addr.trim().to_ascii_uppercase().as_str(),
            "YES" | "Y" | "1" | "TRUE"
        );
        let env_addrs = std::env::var("EPICS_PVA_ADDR_LIST").ok();
        // PVA-466: expand $(VAR) before checking emptiness so an
        // unset macro collapses to "" and the no-destinations
        // warning fires correctly.
        let env_has_dest = env_addrs
            .as_deref()
            .map(|s| crate::config::env::expand_dollar_vars(s))
            .map(|s| {
                s.split(|c: char| c == ',' || c.is_whitespace())
                    .any(|t| !t.trim().is_empty())
            })
            .unwrap_or(false);
        if extra_targets.is_empty() && !env_has_dest && !auto_on {
            tracing::warn!(
                target: "epics_pva_rs::client",
                "PVA client context created with no search destinations \
                 (EPICS_PVA_ADDR_LIST empty, EPICS_PVA_AUTO_ADDR_LIST=NO, \
                 no programmatic addr_list). All searches will time out."
            );
        }

        // Resolve EPICS_PVA_ADDR_LIST once at startup and merge into
        // `extra_targets`. Uses the shared parser (`parse_addr_list_with_port`)
        // so `IP`, `IP:port`, `hostname`, and `hostname:port` all work —
        // previously the search engine only handled literal IPs and
        // silently dropped DNS hostnames, mirroring the pre-fix libca
        // bug captured in `parse_addr_list_with_port`'s P-6 comment.
        // DNS is blocking; offload to the blocking pool so a slow
        // resolver doesn't stall the engine spawn on the runtime's
        // worker thread for the full DNS timeout.
        if let Some(s) = env_addrs.as_deref() {
            let s = s.to_string();
            let bport = crate::config::env::broadcast_port();
            let resolved = tokio::task::spawn_blocking(move || {
                crate::config::env::parse_addr_list_with_port(&s, bport)
            })
            .await
            .unwrap_or_default();
            // PR #205 IPv6 Stage 4: v6 entries are now routable
            // when `search_socket_v6` was bound successfully. If the
            // host has no v6 stack (v6 bind failed) we still need to
            // drop v6 entries to avoid the InvalidInput retry storm
            // on the v4 socket, but emit a one-shot warning rather
            // than silently skipping.
            let v6_available = search_socket_v6.is_some();
            let mut dropped_v6: Vec<SocketAddr> = Vec::new();
            for sa in resolved {
                if matches!(sa, SocketAddr::V6(_)) && !v6_available {
                    dropped_v6.push(sa);
                    continue;
                }
                if !extra_targets.contains(&sa) {
                    extra_targets.push(sa);
                }
            }
            if !dropped_v6.is_empty() {
                warn!(
                    dropped = ?dropped_v6,
                    "EPICS_PVA_ADDR_LIST contained IPv6 entries but no usable v6 \
                     socket could be bound on this host. Dropping these entries; \
                     the rest of the search remains IPv4-only."
                );
            }
            // When v6 is available and AUTO_ADDR_LIST is YES, add
            // the default v6 multicast group (ff0e::400:5076). Mirrors
            // the v4 path that adds `224.0.2.3` only on explicit
            // opt-in via ADDR_LIST — but for v6 we have no equivalent
            // limited broadcast, so the multicast group is the only
            // way to reach v6-only servers without enumerating each.
            if v6_available && auto_on {
                let bport = crate::config::env::broadcast_port();
                let mcast = SocketAddr::V6(std::net::SocketAddrV6::new(
                    DEFAULT_V6_MULTICAST_GROUP,
                    bport,
                    0,
                    0,
                ));
                if !extra_targets.contains(&mcast) {
                    extra_targets.push(mcast);
                }
            }
        }

        let beacons_clone = beacons.clone();
        tokio::spawn(run_engine(
            cmd_rx,
            search_socket,
            search_socket_v6,
            beacon_socket,
            beacon_socket_v6,
            extra_targets,
            beacons_clone,
            name_servers,
        ));

        Ok(Self { cmd_tx, beacons })
    }

    /// Issue a search for `pv_name`. Future resolves to the server address
    /// once a response arrives. `reason` controls whether the first
    /// SEARCH packet fires immediately (`Initial`) or is bucket-spread
    /// (`Reconnect`).
    pub async fn find(&self, pv_name: &str, reason: SearchReason) -> PvaResult<SocketAddr> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SearchCommand::Find {
                pv_name: pv_name.to_string(),
                responder: tx,
                reason,
            })
            .await
            .map_err(|_| PvaError::Protocol("search engine closed".into()))?;
        rx.await
            .map_err(|_| PvaError::Protocol("search request cancelled".into()))
    }

    /// Collect every SEARCH_RESPONSE for `pv_name` within
    /// [`MULTI_SERVER_WINDOW`]. Returns a ranked list — first is the
    /// fastest responder. Empty list means the search timed out.
    pub async fn find_all(
        &self,
        pv_name: &str,
        reason: SearchReason,
    ) -> PvaResult<Vec<SocketAddr>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SearchCommand::FindAll {
                pv_name: pv_name.to_string(),
                responder: tx,
                reason,
            })
            .await
            .map_err(|_| PvaError::Protocol("search engine closed".into()))?;
        rx.await
            .map_err(|_| PvaError::Protocol("search request cancelled".into()))
    }

    pub async fn cancel(&self, pv_name: &str) {
        let _ = self
            .cmd_tx
            .send(SearchCommand::Cancel {
                pv_name: pv_name.to_string(),
            })
            .await;
    }

    pub async fn observe_beacon(&self, server: SocketAddr, guid: [u8; 12]) {
        let _ = self
            .cmd_tx
            .send(SearchCommand::BeaconObserved { server, guid })
            .await;
    }

    /// Most recent GUID this engine's BeaconTracker has observed for
    /// `addr`. Used by Channel::ensure_active to detect server
    /// replacement at the same address (P-G12). None when the
    /// address has never produced a beacon (or we have no beacon
    /// listener for it).
    pub fn beacon_guid_for(&self, addr: SocketAddr) -> Option<[u8; 12]> {
        self.beacons.guid_for(addr)
    }

    /// Force the engine into fast-tick mode (200 ms × 30 ticks ≈ 6 s)
    /// and reset every pending search's retry deadline. Equivalent to
    /// pvxs `Context::hurryUp`: lets an application kick all pending
    /// searches when it has out-of-band evidence the network changed
    /// (link bounce, new IOC announced over a side channel, etc.).
    pub async fn hurry_up(&self) {
        let _ = self.cmd_tx.send(SearchCommand::HurryUp).await;
    }

    /// Drop any cached state for `pv_name` — cancels its outstanding
    /// search and removes the name → search-id mapping. The next
    /// `find()` re-runs from scratch. Mirrors pvxs `cacheClear`.
    pub async fn cache_clear(&self, pv_name: &str) {
        let _ = self
            .cmd_tx
            .send(SearchCommand::CacheClear {
                pv_name: pv_name.to_string(),
            })
            .await;
    }

    /// Set the server-GUID blocklist. Beacons and search responses
    /// from any server whose GUID is on this list are silently
    /// ignored. Mirrors pvxs `Context::ignoreServerGUIDs`.
    pub async fn ignore_server_guids(&self, guids: Vec<[u8; 12]>) {
        let _ = self
            .cmd_tx
            .send(SearchCommand::IgnoreServerGuids { guids })
            .await;
    }

    /// Send a discover ping to broadcast targets — actively solicit
    /// SEARCH_RESPONSE from every reachable server. Pair with
    /// [`Self::discover`] to get `Discovered::Online` events without
    /// waiting for the next beacon. Mirrors pvxs
    /// `DiscoverBuilder::pingAll`.
    pub async fn ping_all(&self) {
        let _ = self.cmd_tx.send(SearchCommand::DiscoverPing).await;
    }

    /// Subscribe to beacon-driven discovery events. The receiver yields a
    /// [`Discovered::Online`] for every (server, guid) pair the
    /// [`BeaconTracker`] regards as new or restarted. Mirrors pvxs's
    /// `client::Context::discover()` callback API.
    ///
    /// The receiver is bounded; if the consumer falls behind, events are
    /// dropped silently. Drop the receiver to unsubscribe.
    pub async fn discover(&self) -> PvaResult<mpsc::Receiver<Discovered>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SearchCommand::Subscribe { responder: tx })
            .await
            .map_err(|_| PvaError::Protocol("search engine closed".into()))?;
        rx.await
            .map_err(|_| PvaError::Protocol("subscribe cancelled".into()))
    }
}

// ── UDP socket helpers ──────────────────────────────────────────────────

fn bind_ephemeral_udp() -> PvaResult<AsyncUdpV4> {
    // SEARCH packets embed a `response_port` that IOCs reply unicast
    // to. With per-NIC sockets we want every NIC's reply port to be
    // identical so the IOC's response lands on the right
    // logical socket regardless of which NIC delivered it back.
    AsyncUdpV4::bind_ephemeral_same_port(true).map_err(PvaError::Io)
}

/// PR #205 IPv6 Stage 4: bind a v6-only ephemeral UDP socket used to
/// send SEARCH to IPv6 destinations (unicast servers from
/// `EPICS_PVA_ADDR_LIST` and the v6 multicast group). Returns `None`
/// when the host lacks IPv6 (bind fails) — the engine keeps running
/// IPv4-only in that case.
///
/// Sets `IPV6_V6ONLY=true` explicitly so the v6 socket cannot accept
/// v4-mapped traffic that would otherwise duplicate what the
/// `AsyncUdpV4` search socket already handles.
fn bind_ephemeral_udp_v6() -> Option<Arc<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let sock = match Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            debug!("IPv6 SEARCH socket: socket() failed: {e}; v6 disabled");
            return None;
        }
    };
    if let Err(e) = sock.set_only_v6(true) {
        debug!("IPv6 SEARCH socket: set_only_v6 failed: {e}");
    }
    if let Err(e) = sock.set_nonblocking(true) {
        debug!("IPv6 SEARCH socket: set_nonblocking failed: {e}");
        return None;
    }
    // Multicast TTL=1 (link-local only) is the safe default — matches
    // pvxs `udp_collector.cpp` v6 send path.
    if let Err(e) = sock.set_multicast_hops_v6(1) {
        debug!("IPv6 SEARCH socket: set_multicast_hops_v6 failed: {e}");
    }
    let bind =
        std::net::SocketAddr::V6(std::net::SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
    if let Err(e) = sock.bind(&bind.into()) {
        debug!("IPv6 SEARCH socket: bind {bind} failed: {e}; v6 disabled");
        return None;
    }
    let std_sock: std::net::UdpSocket = sock.into();
    match UdpSocket::from_std(std_sock) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            debug!("IPv6 SEARCH socket: tokio adoption failed: {e}");
            None
        }
    }
}

/// `select!`-friendly recv helper: yields the next datagram from the
/// optional v6 socket, or `None` (forever-pending) when v6 is disabled.
/// Matches the pattern used for the optional beacon socket.
async fn recv_from_v6_opt(
    sock: Option<&Arc<UdpSocket>>,
    buf: &mut [u8],
) -> Option<std::io::Result<(usize, SocketAddr)>> {
    match sock {
        Some(s) => Some(s.recv_from(buf).await),
        None => std::future::pending().await,
    }
}

fn bind_beacon_udp() -> Option<AsyncUdpV4> {
    let port = std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_BROADCAST_PORT);
    // Skip the loopback NIC: any local pva-rs *server* has its UDP
    // responder bound on 127.0.0.1:5076 with SO_REUSEPORT, and a
    // co-bound client beacon socket on the same (addr, port) would
    // race with the server for inbound SEARCH packets via the kernel's
    // REUSEPORT load-balancing on macOS / Linux. Beacons that the
    // local server emits go to NIC subnet broadcasts (never to the
    // loopback addr — see `config::env::list_broadcast_addresses`
    // which filters loopback), so dropping the loopback bind here
    // costs nothing on the receive side.
    let sock = match AsyncUdpV4::bind_non_loopback(port, true) {
        Ok(s) => s,
        Err(e) => {
            debug!("beacon socket bind to {port} failed: {e}; fast-reconnect disabled");
            return None;
        }
    };
    // pvxs udp_collector.cpp:140 binds wildcard so we also receive
    // multicast packets — but only for groups we've explicitly joined.
    // Join any multicast groups present in EPICS_PVA_ADDR_LIST (and the
    // standard PVA `224.0.2.3` group is left to user opt-in to avoid
    // surprising multicast traffic from a default config).
    join_addr_list_multicast(&sock);
    Some(sock)
}

/// PR #205 IPv6 Stage 6: bind a v6 beacon listener on `[::]:port` with
/// `SO_REUSEADDR`/`SO_REUSEPORT` + `IPV6_V6ONLY=1`, then join the
/// default v6 PVA multicast group `ff0e::400`. Returns `None` when the
/// host lacks v6 or the bind fails — fast-reconnect via v6 beacons is
/// best-effort, the v4 beacon socket keeps doing its job.
fn bind_beacon_udp_v6() -> Option<Arc<UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let port = std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_BROADCAST_PORT);

    let sock = match Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            debug!("v6 beacon socket: socket() failed: {e}; v6 beacon recv disabled");
            return None;
        }
    };
    if let Err(e) = sock.set_only_v6(true) {
        debug!("v6 beacon socket: set_only_v6 failed: {e}");
    }
    // Mirror the v4 beacon socket setup: SO_REUSEADDR + SO_REUSEPORT so
    // a server v6 SEARCH listener on `[::]:port` and a client v6 beacon
    // listener can coexist. The server emits beacons FROM a separate
    // ephemeral socket, so this REUSEPORT-shared listener only receives.
    #[cfg(not(windows))]
    if let Err(e) = sock.set_reuse_address(true) {
        debug!("v6 beacon socket: set_reuse_address failed: {e}");
    }
    #[cfg(unix)]
    if let Err(e) = sock.set_reuse_port(true) {
        debug!("v6 beacon socket: set_reuse_port failed: {e}");
    }
    if let Err(e) = sock.set_nonblocking(true) {
        debug!("v6 beacon socket: set_nonblocking failed: {e}");
        return None;
    }

    let bind = SocketAddr::V6(std::net::SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        port,
        0,
        0,
    ));
    if let Err(e) = sock.bind(&bind.into()) {
        debug!("v6 beacon socket: bind {bind} failed: {e}; v6 beacon recv disabled");
        return None;
    }

    let std_sock: std::net::UdpSocket = sock.into();
    let tokio_sock = match UdpSocket::from_std(std_sock) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            debug!("v6 beacon socket: tokio adoption failed: {e}");
            return None;
        }
    };

    // Join the default PVA v6 multicast group on the unspecified
    // interface (let the OS pick). Additional groups from
    // EPICS_PVA_ADDR_LIST are joined separately.
    if let Err(e) = tokio_sock.join_multicast_v6(&DEFAULT_V6_MULTICAST_GROUP, 0) {
        debug!(
            "v6 beacon socket: join_multicast_v6 ff0e::400 failed: {e}; \
             v6 multicast beacons will not be received"
        );
    }
    Some(tokio_sock)
}

/// Walk `EPICS_PVA_ADDR_LIST` and join every IPv4 multicast group on
/// every up, non-loopback NIC of `sock`. Errors are logged but not
/// propagated — a single failed join shouldn't disable the rest of
/// the discovery path.
pub(crate) fn join_addr_list_multicast(sock: &AsyncUdpV4) {
    let Ok(env) = std::env::var("EPICS_PVA_ADDR_LIST") else {
        return;
    };
    // PVA-466: expand $(VAR) so multicast joins for templated addrs
    // (e.g. EPICS_PVA_ADDR_LIST="$(MCAST_GROUP):5076") resolve
    // consistently with the active-SEARCH path which already
    // expands via parse_addr_list_with_port.
    let env = crate::config::env::expand_dollar_vars(&env);
    for tok in env.split(|c: char| c == ',' || c.is_whitespace()) {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let ip_str = tok.split(':').next().unwrap_or(tok);
        let Ok(ip) = ip_str.parse::<Ipv4Addr>() else {
            continue;
        };
        if ip.is_multicast() {
            if let Err(e) = sock.join_multicast_v4(ip) {
                debug!("join_multicast_v4 for {ip} failed: {e}");
            } else {
                debug!("joined multicast group {ip}");
            }
        }
    }
}

// ── Engine main loop ────────────────────────────────────────────────────

enum Responder {
    Single(oneshot::Sender<SocketAddr>),
    Multi {
        responder: oneshot::Sender<Vec<SocketAddr>>,
        accumulated: Vec<SocketAddr>,
        deadline: Instant,
    },
}

struct Pending {
    pv_name: String,
    responder: Responder,
    last_attempt: Instant,
    /// Number of times this search has been broadcast. 0 before the
    /// first transmit; bumped to 1 after the bucket-fire (or
    /// immediate-fire for `Initial`). Doubles as the pvxs `nSearch`
    /// counter that controls retry-bucket escalation: each retry
    /// pushes the search forward by `min(attempt, nBuckets)` buckets,
    /// giving the 1 s, 2 s, 3 s, ..., 30 s pattern.
    attempt: u32,
    /// Which search bucket this pending currently occupies.
    bucket: usize,
}

/// pvxs `client.cpp::nBuckets`. 30 buckets at 1 s normal interval gives
/// each pending search a 30-second slot rotation — cooperative tick
/// caps UDP search traffic at roughly `pending.len() / 30` packets per
/// second instead of letting every channel fire on its own backoff.
const N_SEARCH_BUCKETS: usize = 30;

/// Decide which bucket to drop a fresh search into based on the
/// caller's intent. Pure function so the production handlers and
/// the unit tests share the formula and can't drift apart.
///
/// - `Initial`: `current_bucket + 1`. The Find handler ALSO fires an
///   immediate broadcast for `Initial`; the +1 placement is so the
///   first scheduled retry lands one tick after the immediate fire
///   instead of being eaten by the same tick the immediate broadcast
///   already covered.
/// - `Reconnect`: `current_bucket`. Mirrors pvxs `Channel::disconnect`
///   (client.cpp:213) which pushes the channel into
///   `searchBuckets[currentBucket + holdoff]` with `holdoff = 0` for
///   the typical Active→disconnect case. The next 1 Hz tick takes
///   the current bucket and broadcasts; latency ≤ 1 s.
///
/// Cascade-spread (5000 channels disconnecting simultaneously) is
/// handled by the natural O(N / nBuckets) per-tick rate-limit and
/// the runtime-side smoothing in `cascade_smoothed_next` — no
/// per-channel sid hashing needed.
fn placement_bucket(current_bucket: usize, reason: SearchReason) -> usize {
    match reason {
        SearchReason::Initial => (current_bucket + 1) % N_SEARCH_BUCKETS,
        SearchReason::Reconnect => current_bucket,
    }
}

/// Compute the next-retry bucket for a search that just transmitted.
/// Mirrors pvxs `tickSearch` (client.cpp:1193-1206):
///
///   `next = (idx + nSearch) % nBuckets`, where `nSearch` is the
///   per-channel attempt counter, capped at `nBuckets`. Each retry
///   pushes the search forward by one more bucket, which gives a
///   gradually-escalating backoff: 1 s, 2 s, 3 s, ..., capping at
///   the 30 s ring period.
///
/// Cascade smoothing (line 1199-1206 in pvxs): when the chosen
/// `next` bucket is overloaded relative to the bucket immediately
/// after it (>100 entries more), defer to that one. Distributes a
/// mass-disconnect across two ticks instead of one. The asymmetry
/// matters only at runtime when the bucket sizes are observable;
/// the placement formula stays a pure function of bucket counts.
///
/// `attempt` is 1-based (1 means "this is the first retransmit
/// after the initial bucket-fire").
fn cascade_smoothed_next(
    current_bucket: usize,
    attempt: u32,
    bucket_sizes: impl Fn(usize) -> usize,
) -> usize {
    let n_search = (attempt as usize).min(N_SEARCH_BUCKETS);
    let next = (current_bucket + n_search) % N_SEARCH_BUCKETS;
    let nextnext = (next + 1) % N_SEARCH_BUCKETS;
    let next_n = bucket_sizes(next);
    let nextnext_n = bucket_sizes(nextnext);
    // pvxs only smooths when `nextN > nextnextN AND difference > 100`
    // — i.e. the imbalance is large enough that one tick of deferral
    // visibly evens things out. With the difference < 100 it's fine
    // to leave the work in `next`.
    if next_n > nextnext_n && next_n - nextnext_n > 100 {
        nextnext
    } else {
        next
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_engine(
    mut cmd_rx: mpsc::Receiver<SearchCommand>,
    search_socket: AsyncUdpV4,
    search_socket_v6: Option<Arc<UdpSocket>>,
    beacon_socket: Option<AsyncUdpV4>,
    beacon_socket_v6: Option<Arc<UdpSocket>>,
    extra_targets: Vec<SocketAddr>,
    beacons: Arc<BeaconTracker>,
    name_servers: Vec<SocketAddr>,
) {
    static NEXT_SEARCH_ID: AtomicU32 = AtomicU32::new(1);

    let codec = PvaCodec { big_endian: false };
    // All NICs share one ephemeral port (bind_ephemeral_same_port), so
    // any per-NIC socket gives the same answer.
    let response_port = search_socket
        .local_addrs()
        .first()
        .map(|a| a.port())
        .unwrap_or(0);

    let mut pending: HashMap<u32, Pending> = HashMap::new(); // by search_id
    let mut by_name: HashMap<String, u32> = HashMap::new(); // pv_name → search_id
    let mut subscribers: Vec<mpsc::Sender<Discovered>> = Vec::new();
    // Search bucket ring (pvxs client.cpp searchBuckets[30]). Each
    // bucket holds the search_ids whose retry slot is "this bucket"
    // on the rotating cursor. Tick advances the cursor and processes
    // exactly one bucket — so steady-state UDP search load = O(1) per
    // tick regardless of how many channels are pending.
    let mut search_buckets: Vec<Vec<u32>> = vec![Vec::new(); N_SEARCH_BUCKETS];
    let mut current_bucket: usize = 0;
    // Server-GUID blocklist (pvxs `ignoreServerGUIDs`). Beacons and
    // search responses with a matching GUID are silently dropped.
    // HashSet lookup keeps the steady-state cost negligible.
    let mut ignore_guids: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
    // After a `poke()` (fresh server identity discovered) we run one
    // 30-bucket revolution at fast 200 ms cadence so all pending
    // searches retry within 6 s instead of up to 30 s. Counter
    // decrements per fast tick; reaches 0 → revert to 1 s cadence.
    let mut fast_ticks_remaining: u32 = 0;
    // (server, guid) pairs already announced via discover(). pvxs's
    // discover() fires Online once per new server identity; tracker
    // uses different (reconnect-throttle) semantics so we de-dup here.
    let mut announced: std::collections::HashSet<(SocketAddr, [u8; 12])> =
        std::collections::HashSet::new();

    // pvxs client.cpp:651-667 startNS(): one persistent TCP connection per
    // EPICS_PVA_NAME_SERVERS entry. Each ns_task handles connect/reconnect;
    // ns_senders receives SEARCH frame bytes to forward over the connection.
    let (ns_response_tx, mut ns_response_rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(64);
    let mut ns_senders: Vec<mpsc::Sender<Vec<u8>>> = Vec::with_capacity(name_servers.len());
    for ns_addr in name_servers {
        let (search_tx, search_rx) = mpsc::channel::<Vec<u8>>(64);
        ns_senders.push(search_tx);
        let resp_tx = ns_response_tx.clone();
        tokio::spawn(ns_task(ns_addr, search_rx, resp_tx));
    }

    let mut tick = interval(Duration::from_secs(1));
    // Periodic beacon-tracker cleanup: every BEACON_CLEAN_INTERVAL we
    // walk the map and forget servers whose beacons have been silent
    // longer than BEACON_TIMEOUT. Each pruned entry fires a
    // `Discovered::Timeout`. Mirrors pvxs tickBeaconClean.
    let mut beacon_clean_tick = interval(BEACON_CLEAN_INTERVAL);
    beacon_clean_tick.tick().await; // skip immediate fire
    // 64 KB UDP receive buffers — IPv4 maximum. Search responses
    // can be chained (multiple SEARCH replies per datagram) and
    // beacons can include large server-hello payloads on TLS-aware
    // servers; the previous 4 KB cap silently truncated either case.
    // Matches the new server-side recv buffer (server_native/udp.rs).
    let mut search_buf = vec![0u8; 64 * 1024];
    let mut search_buf_v6 = vec![0u8; 64 * 1024];
    let mut beacon_buf = vec![0u8; 64 * 1024];
    let mut beacon_buf_v6 = vec![0u8; 64 * 1024];
    let mut search_send_errs: HashSet<SocketAddr> = HashSet::new();

    loop {
        // Build a beacon-recv future regardless of whether we bound it
        // (using `if let` to keep the select! shape simple).
        let beacon_recv = async {
            match &beacon_socket {
                Some(s) => s.recv_from(&mut beacon_buf).await,
                None => std::future::pending().await,
            }
        };

        // Earliest unfired `Multi` (FindAll) deadline. Without this,
        // the only place deadlines get flushed is the 1 Hz `tick`
        // arm, so a SEARCH_RESPONSE that arrives at e.g. 5 ms still
        // makes the caller wait the rest of the second for the next
        // tick before `find_all` returns. Sleep precisely until the
        // earliest deadline so the common single-server case
        // resolves in `MULTI_SERVER_WINDOW` (200 ms) — not in
        // whatever fraction of the 1 s tick remains.
        let next_multi_deadline: Option<Instant> = pending
            .values()
            .filter_map(|p| match &p.responder {
                Responder::Multi { deadline, .. } if p.attempt > 0 => Some(*deadline),
                _ => None,
            })
            .min();
        let deadline_arm = async {
            match next_multi_deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(SearchCommand::Find { pv_name, responder, reason }) => {
                    // P-G27: drop any prior pending search for the
                    // same name so a tight retry loop doesn't grow
                    // pending / search_buckets without bound. The
                    // old responder gets dropped (oneshot Sender drops
                    // → caller's find() future returns Cancelled).
                    if let Some(old_sid) = by_name.remove(&pv_name) {
                        if let Some(old) = pending.remove(&old_sid) {
                            search_buckets[old.bucket].retain(|x| *x != old_sid);
                        }
                    }
                    let sid = NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed);
                    let bucket = placement_bucket(current_bucket, reason);
                    search_buckets[bucket].push(sid);
                    let p = Pending {
                        pv_name: pv_name.clone(),
                        responder: Responder::Single(responder),
                        last_attempt: Instant::now(),
                        attempt: 0,
                        bucket,
                    };
                    by_name.insert(pv_name, sid);
                    pending.insert(sid, p);
                    if reason == SearchReason::Initial {
                        if let Some(p) = pending.get_mut(&sid) {
                            // Mark this as an attempt so the bucket-fire
                            // path doesn't later re-anchor the Multi
                            // responder's deadline (it would only do that
                            // when `attempt == 0`, i.e. the bucket is
                            // firing the FIRST broadcast — true for
                            // Reconnect, false for Initial).
                            p.attempt = 1;
                            p.last_attempt = Instant::now();
                            let pkt = codec.build_search(0, sid, &p.pv_name, [0,0,0,0], response_port, false);
                            broadcast(&search_socket, search_socket_v6.as_ref(), &pkt, &extra_targets, &mut search_send_errs).await;
                            // pvxs client.cpp:1193-1196: also SEARCH over each TCP
                            // name-server connection. unicast bit=0x80, port=0 (NS
                            // replies on the same TCP connection).
                            if !ns_senders.is_empty() {
                                let ns_pkt = codec.build_search(0, sid, &p.pv_name, [0, 0, 0, 0], 0, true);
                                for tx in &ns_senders {
                                    let _ = tx.try_send(ns_pkt.clone());
                                }
                            }
                        }
                    }
                }
                Some(SearchCommand::FindAll { pv_name, responder, reason }) => {
                    // P-G27: same dedup as Find — drop any prior
                    // pending search for the same name.
                    if let Some(old_sid) = by_name.remove(&pv_name) {
                        if let Some(old) = pending.remove(&old_sid) {
                            search_buckets[old.bucket].retain(|x| *x != old_sid);
                        }
                    }
                    let sid = NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed);
                    let bucket = placement_bucket(current_bucket, reason);
                    search_buckets[bucket].push(sid);
                    let p = Pending {
                        pv_name: pv_name.clone(),
                        responder: Responder::Multi {
                            responder,
                            accumulated: Vec::new(),
                            deadline: Instant::now() + MULTI_SERVER_WINDOW,
                        },
                        last_attempt: Instant::now(),
                        attempt: 0,
                        bucket,
                    };
                    by_name.insert(pv_name, sid);
                    pending.insert(sid, p);
                    if reason == SearchReason::Initial {
                        if let Some(p) = pending.get_mut(&sid) {
                            p.attempt = 1;
                            p.last_attempt = Instant::now();
                            let pkt = codec.build_search(0, sid, &p.pv_name, [0,0,0,0], response_port, false);
                            broadcast(&search_socket, search_socket_v6.as_ref(), &pkt, &extra_targets, &mut search_send_errs).await;
                            if !ns_senders.is_empty() {
                                let ns_pkt = codec.build_search(0, sid, &p.pv_name, [0, 0, 0, 0], 0, true);
                                for tx in &ns_senders {
                                    let _ = tx.try_send(ns_pkt.clone());
                                }
                            }
                        }
                    }
                }
                Some(SearchCommand::Cancel { pv_name }) => {
                    if let Some(sid) = by_name.remove(&pv_name) {
                        if let Some(p) = pending.remove(&sid) {
                            search_buckets[p.bucket].retain(|x| *x != sid);
                        }
                    }
                }
                Some(SearchCommand::BeaconObserved { server, guid }) => {
                    if ignore_guids.contains(&guid) {
                        continue;
                    }
                    let allow_reconnect = beacons.observe(server, guid);
                    // discover() de-dup: announce each (server, guid) pair
                    // exactly once until forgotten.
                    let first_announce = announced.insert((server, guid));
                    // pvxs `poke()` semantics: only kick pending searches
                    // when the server identity is FRESH — either a
                    // brand-new (server, guid) pair, or the same server
                    // returning with a new GUID after the anomaly window.
                    // Without the `first_announce` gate every periodic
                    // beacon would needlessly bring forward every pending
                    // search's retry deadline.
                    if allow_reconnect && first_announce {
                        // pvxs `poke()` (client.cpp:713). Switch the tick
                        // ring to 200 ms cadence for one full revolution
                        // (30 ticks ≈ 6 s) so every pending search retries
                        // quickly without permanently spamming the net.
                        if fast_ticks_remaining == 0 {
                            tick = interval(Duration::from_millis(200));
                            tick.tick().await; // skip immediate fire
                        }
                        fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                        for p in pending.values_mut() {
                            p.last_attempt = Instant::now() - Duration::from_secs(60);
                            // Reset `attempt` so the kicked retry
                            // re-enters at the 1-bucket forward push
                            // instead of inheriting the prior
                            // escalation. NOTE: this is MORE aggressive
                            // than pvxs `tickSearch` line 1194
                            // (`if !poked`), which skips the nSearch
                            // increment for one tick (search re-pushed
                            // to SAME bucket). Our reset-to-0 means
                            // post-poke retries cascade at the normal
                            // 1, 2, 3, … bucket-forward pattern from
                            // scratch, giving rapid retransmits during
                            // the fast-tick window. Acceptable trade
                            // for single-channel recovery; under
                            // mass-disconnect cascades it spends more
                            // UDP bandwidth than pvxs would.
                            p.attempt = 0;
                        }
                    }
                    if first_announce {
                        // BeaconObserved is the in-process injection
                        // path (e.g., a co-located server) — there's
                        // no UDP datagram, so peer == server and proto
                        // defaults to "tcp". Real beacons go through
                        // handle_beacon below where the proto string
                        // is parsed off the wire.
                        let evt = Discovered::Online {
                            server,
                            guid,
                            peer: server,
                            proto: "tcp".into(),
                        };
                        subscribers.retain(|tx| tx.try_send(evt.clone()).is_ok());
                    }
                }
                Some(SearchCommand::Subscribe { responder }) => {
                    let (tx, rx) = mpsc::channel::<Discovered>(64);
                    subscribers.push(tx);
                    let _ = responder.send(rx);
                }
                Some(SearchCommand::HurryUp) => {
                    // Same effect as a fresh-server beacon: switch to
                    // fast-tick mode for one revolution and reset
                    // every pending search's retry counter so they
                    // all retry within ~6 s. SEE NOTE in the
                    // BeaconObserved arm above — our `attempt = 0`
                    // reset is more aggressive than pvxs's `poked`
                    // semantic (which preserves nSearch and just
                    // skips the increment for one tick). Tradeoff is
                    // documented there.
                    if fast_ticks_remaining == 0 {
                        tick = interval(Duration::from_millis(200));
                        tick.tick().await;
                    }
                    fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    let now = Instant::now();
                    for p in pending.values_mut() {
                        p.last_attempt = now - Duration::from_secs(60);
                        p.attempt = 0;
                    }
                }
                Some(SearchCommand::CacheClear { pv_name }) => {
                    // Same drop-the-name path as Cancel, but the name
                    // is the public identifier.
                    if let Some(sid) = by_name.remove(&pv_name) {
                        if let Some(p) = pending.remove(&sid) {
                            search_buckets[p.bucket].retain(|x| *x != sid);
                        }
                    }
                }
                Some(SearchCommand::IgnoreServerGuids { guids }) => {
                    // Replace (not merge) so callers can also CLEAR
                    // the list with an empty Vec. Drop tracker entries
                    // we now want to ignore so a stale GUID doesn't
                    // keep firing throttle decisions.
                    ignore_guids = guids.into_iter().collect();
                    if !ignore_guids.is_empty() {
                        announced.retain(|(_, g)| !ignore_guids.contains(g));
                    }
                }
                Some(SearchCommand::DiscoverPing) => {
                    // pvxs DiscoverBuilder::pingAll wire format
                    // (client.cpp:1054-1074): empty SEARCH with
                    // `MustReply` flag, zero protocols, zero channels.
                    // Every reachable PVA server replies regardless
                    // of whether it claims any specific PV name. The
                    // earlier `build_search("")` call produced a
                    // single-channel SEARCH with empty name, which
                    // most servers correctly ignored as malformed —
                    // `ping_all()` was effectively a silent op.
                    let probe_id = NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed);
                    let pkt = codec.build_discover_search(probe_id, response_port);
                    broadcast(&search_socket, search_socket_v6.as_ref(), &pkt, &extra_targets, &mut search_send_errs).await;
                }
                None => break,
            },

            res = search_socket.recv_from(&mut search_buf) => {
                if let Ok((n, peer)) = res {
                    // Multi-message drain (P-G10): pvxs packs many
                    // SEARCH messages per UDP datagram. Without the
                    // loop we'd parse only the first and silently
                    // drop the rest.
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_search_response(
                            &search_buf[pos..n],
                            &mut pending, &mut by_name, &beacons, &ignore_guids, peer, false,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                }
            }

            res = recv_from_v6_opt(search_socket_v6.as_ref(), &mut search_buf_v6) => {
                // PR #205 IPv6 Stage 4: v6 SEARCH_RESPONSE arrives
                // unicast back to this v6 socket. Decode reuses the
                // same family-agnostic handler.
                if let Some(Ok((n, peer))) = res {
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_search_response(
                            &search_buf_v6[pos..n],
                            &mut pending, &mut by_name, &beacons, &ignore_guids, peer, false,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                }
            }

            res = beacon_recv => {
                if let Ok((n, from)) = res {
                    let mut poke = false;
                    // Multi-message drain (P-G10): same rationale as
                    // search responses — beacons can be chained.
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_beacon(
                            &beacon_buf[pos..n], &beacons, &mut pending,
                            &mut subscribers, &mut announced, &mut poke,
                            &ignore_guids, from,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke && fast_ticks_remaining == 0 {
                        tick = interval(Duration::from_millis(200));
                        tick.tick().await;
                        fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    } else if poke {
                        // Already in fast mode: extend the revolution.
                        fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    }
                }
            }

            res = recv_from_v6_opt(beacon_socket_v6.as_ref(), &mut beacon_buf_v6) => {
                // PR #205 IPv6 Stage 6: v6 multicast beacon recv arm.
                // Same decode path as the v4 beacon socket — beacon
                // payloads are family-agnostic (server GUID + TCP port
                // + 16-byte IPv4-mapped server address).
                if let Some(Ok((n, from))) = res {
                    let mut poke = false;
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_beacon(
                            &beacon_buf_v6[pos..n], &beacons, &mut pending,
                            &mut subscribers, &mut announced, &mut poke,
                            &ignore_guids, from,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke && fast_ticks_remaining == 0 {
                        tick = interval(Duration::from_millis(200));
                        tick.tick().await;
                        fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    } else if poke {
                        fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    }
                }
            }

            _ = beacon_clean_tick.tick() => {
                for (server, guid) in beacons.prune_stale(BEACON_TIMEOUT) {
                    announced.remove(&(server, guid));
                    let evt = Discovered::Timeout { server, guid };
                    subscribers.retain(|tx| tx.try_send(evt.clone()).is_ok());
                }
            }

            ns_rsp = ns_response_rx.recv() => {
                // SEARCH_RESPONSE received over a TCP name-server connection.
                // pvxs client.cpp:984-995: procSearchReply with istcp=true.
                if let Some((bytes, ns_addr)) = ns_rsp {
                    handle_search_response(
                        &bytes, &mut pending, &mut by_name, &beacons, &ignore_guids, ns_addr, true,
                    );
                }
            }

            _ = deadline_arm => {
                // The earliest unfired `Multi` deadline elapsed —
                // flush it (and any others now past). Without this
                // arm, deadlines were only checked at the 1 Hz tick,
                // so a SEARCH_RESPONSE that arrived in 5 ms still
                // sat in `accumulated` until the next tick (up to
                // 1 s of dead time). With it the common single-
                // server case resolves at `MULTI_SERVER_WINDOW`
                // (200 ms), regardless of where the tick happens
                // to fall.
                flush_expired_pending(
                    &mut pending,
                    &mut by_name,
                    &mut search_buckets,
                );
            }

            _ = tick.tick() => {
                let now = Instant::now();

                // 1. Flush expired FindAll multi-window responders.
                //    Same logic the deadline arm runs — covers the
                //    case where deadline_arm was racing against a
                //    just-armed entry and missed it on this iteration.
                //
                //    Closed Single responders (caller dropped the
                //    find() future via outer timeout / abort) are
                //    cleaned up in the same pass to keep pending
                //    bounded.
                flush_expired_pending(
                    &mut pending,
                    &mut by_name,
                    &mut search_buckets,
                );

                // 2. Process exactly one search bucket per tick. Each
                //    pending in this bucket gets one UDP retransmit
                //    and is then re-armed into a future bucket using
                //    pvxs's `nSearch+1` escalation (`tickSearch`,
                //    client.cpp:1193-1196):
                //
                //      next = (idx + min(attempt, nBuckets)) % nBuckets
                //
                //    `attempt` is bumped immediately after the send
                //    so the first retry lands at idx+1 (1 s later),
                //    the second at idx+2 (2 s after that), the
                //    third at idx+3 (4 s total), …, capping at
                //    idx+30 (one full ring = 30 s steady-state). The
                //    earlier `holdoff_cycles=10` design conflated
                //    pvxs's pre-CREATE_CHANNEL holdoff with the
                //    Active-disconnect retry path; pvxs only uses
                //    the 10-bucket holdoff for `Channel::Connecting`
                //    drops, never for the steady reconnect cadence.
                //
                //    Cascade smoothing: when the chosen `next` bucket
                //    is overloaded vs `next+1` by 100+ entries, defer
                //    to `next+1` (mirrors pvxs `client.cpp:1199-1206`).
                //    Lets a mass-disconnect spread across two ticks
                //    instead of one.
                let bucket_ids = std::mem::take(&mut search_buckets[current_bucket]);
                for sid in bucket_ids {
                    let responder_dead = match pending.get(&sid) {
                        // F6: drop searches whose oneshot responder
                        // was already closed (caller cancelled their
                        // find() future via outer timeout / abort).
                        // Without this the bucket loop keeps
                        // re-broadcasting dead searches forever.
                        Some(p) => match &p.responder {
                            Responder::Single(tx) => tx.is_closed(),
                            Responder::Multi { responder, .. } => responder.is_closed(),
                        },
                        // Search was deduped or cancelled out from
                        // under us before we got here — nothing to
                        // do.
                        None => true,
                    };
                    if responder_dead {
                        if let Some(p) = pending.remove(&sid) {
                            by_name.remove(&p.pv_name);
                        }
                        continue;
                    }

                    let pkt_opt = pending.get_mut(&sid).map(|p| {
                        // First-broadcast bookkeeping for FindAll
                        // callers: the deadline was set to
                        // `call_time + MULTI_SERVER_WINDOW` assuming
                        // an immediate broadcast. With Reconnect
                        // placement going through the bucket
                        // scheduler, the actual first broadcast can
                        // land up to one tick later. Without re-
                        // anchoring, the accumulation window may
                        // already have expired by the time SEARCH
                        // actually goes out and post-first responses
                        // get dropped. Re-arm relative to NOW on
                        // the first attempt only.
                        if p.attempt == 0 {
                            if let Responder::Multi { ref mut deadline, .. } = p.responder {
                                *deadline = now + MULTI_SERVER_WINDOW;
                            }
                        }
                        p.last_attempt = now;
                        p.attempt = p.attempt.saturating_add(1);
                        codec.build_search(
                            0, sid, &p.pv_name, [0, 0, 0, 0], response_port, false,
                        )
                    });
                    if let Some(pkt) = pkt_opt {
                        broadcast(&search_socket, search_socket_v6.as_ref(), &pkt, &extra_targets, &mut search_send_errs)
                            .await;
                        // Re-arm into the escalation bucket. Read
                        // attempt under a fresh borrow so the closure
                        // above doesn't have to outlive the
                        // search_buckets borrow we need below.
                        if let Some(p) = pending.get(&sid) {
                            let attempt = p.attempt;
                            // pvxs client.cpp:1193-1196: TCP name-server SEARCH.
                            // unicast bit=0x80, port=0 (replies on same TCP connection).
                            if !ns_senders.is_empty() {
                                let ns_pkt = codec.build_search(0, sid, &p.pv_name, [0, 0, 0, 0], 0, true);
                                for tx in &ns_senders {
                                    let _ = tx.try_send(ns_pkt.clone());
                                }
                            }
                            let bucket_sizes = |idx: usize| search_buckets[idx].len();
                            let next = cascade_smoothed_next(
                                current_bucket,
                                attempt,
                                bucket_sizes,
                            );
                            // Update the pending's bucket BEFORE the
                            // mutable push so the in-place state and
                            // the buckets agree if we re-enter.
                            if let Some(p) = pending.get_mut(&sid) {
                                p.bucket = next;
                            }
                            search_buckets[next].push(sid);
                        }
                    }
                }
                current_bucket = (current_bucket + 1) % N_SEARCH_BUCKETS;

                // 3. Drop fast-tick mode after one full revolution so we
                //    don't permanently spam the network at 200 ms.
                if fast_ticks_remaining > 0 {
                    fast_ticks_remaining -= 1;
                    if fast_ticks_remaining == 0 {
                        tick = interval(Duration::from_secs(1));
                        // Skip the immediate fire so the new cadence
                        // doesn't double-tick in the same instant.
                        tick.tick().await;
                    }
                }
            }
        }
    }
}

async fn broadcast(
    socket: &AsyncUdpV4,
    socket_v6: Option<&Arc<UdpSocket>>,
    packet: &[u8],
    extra_targets: &[SocketAddr],
    send_errs: &mut HashSet<SocketAddr>,
) {
    let mut targets: Vec<SocketAddr> = Vec::with_capacity(8);

    // Limited broadcast to default UDP port.
    let bport = std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_BROADCAST_PORT);
    targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), bport));

    // pvxs `clientconfig.cpp::expand` parity: when EPICS_PVA_AUTO_ADDR_LIST
    // is YES (the default), enumerate every up-non-loopback NIC's IPv4
    // broadcast address and add it to the search target list. Without
    // this, on multi-NIC hosts (and macOS in particular) we send only
    // to 255.255.255.255 — which the kernel may not translate to the
    // NIC's per-subnet broadcast in all cases, so SEARCHes never reach
    // local IOCs that happen to be listening on `192.168.X.255:5076`.
    // Symptom: `pvget-rs <PV>` against a local pva-rs server hangs
    // until first SEARCH timeout while pvxs `pvget` connects fine.
    if crate::config::env::auto_addr_list_enabled() {
        for sa in crate::config::env::list_broadcast_addresses(bport) {
            // The helper appends 255.255.255.255 as a fallback — we
            // already pushed it above; the post-loop dedup catches it.
            targets.push(sa);
        }
        // Defensive deviation from pvxs: also add `127.0.0.1:port`
        // explicitly. pvxs and EPICS convention rely on the local IOC
        // also binding the NIC broadcast addr (so a NIC-broadcast
        // SEARCH reaches it via `192.168.X.255`). That breaks down on
        // hosts with no usable NIC (CI containers, isolated dev VMs,
        // build sandboxes — anywhere `getifaddrs` returns only
        // loopback). pvxs users hit this and have to set
        // `EPICS_PVA_ADDR_LIST=127.0.0.1` by hand; we send the extra
        // unicast unconditionally to make the zero-config local-IOC
        // workflow work. Cost: one extra UDP datagram per SEARCH
        // burst — negligible.
        targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bport));
    }

    // EPICS_PVA_ADDR_LIST is parsed once at SearchEngine::spawn and
    // merged into `extra_targets` (with DNS hostnames resolved). Per-
    // tick re-reading is redundant and would re-pay the DNS cost on
    // every SEARCH burst.
    for &t in extra_targets {
        targets.push(t);
    }

    // Dedup while preserving insertion order — limited broadcast wins
    // its slot, NIC broadcasts/extras come after.
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert(*t));

    for t in targets {
        // Limited broadcast (255.255.255.255) and multicast (224/4)
        // need explicit per-NIC fanout — OS routing alone would only
        // pick the default-route NIC. Per-subnet broadcast and
        // unicast destinations route via the NIC chosen by AsyncUdpV4.
        // PR #205 IPv6 Stage 4: SocketAddr::V6 destinations are sent
        // via the optional v6 socket. If the engine has no v6 socket
        // the entry was already filtered at spawn time, so reaching
        // this branch here means a programmatic addr_list passed v6
        // through despite the missing socket — fall through to the
        // error path for visibility.
        let result = match t {
            SocketAddr::V4(v4) => {
                let needs_fanout = v4.ip().is_broadcast() || v4.ip().is_multicast();
                if needs_fanout {
                    socket.fanout_to(packet, t).await.map(|_| ())
                } else {
                    socket.send_to(packet, t).await.map(|_| ())
                }
            }
            SocketAddr::V6(_) => match socket_v6 {
                Some(s6) => s6.send_to(packet, t).await.map(|_| ()),
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "no IPv6 search socket; v6 entry routed despite v6 disabled",
                )),
            },
        };
        match result {
            Ok(()) => {
                send_errs.remove(&t);
            }
            Err(e) => {
                if send_errs.insert(t) {
                    warn!("search broadcast to {t} failed: {e}");
                } else {
                    debug!("search broadcast to {t} failed: {e}");
                }
            }
        }
    }
}

/// Flush any pending entries whose Multi deadline has elapsed (deliver
/// the accumulated server list to the caller's oneshot) AND drop any
/// Single entries whose responder has been closed by the caller.
/// Idempotent — safe to call from both the precise deadline-arm path
/// and the fallback 1 Hz tick.
fn flush_expired_pending(
    pending: &mut HashMap<u32, Pending>,
    by_name: &mut HashMap<String, u32>,
    search_buckets: &mut [Vec<u32>],
) {
    let now = Instant::now();
    let mut to_flush_multi = Vec::new();
    let mut to_drop_single = Vec::new();
    for (sid, p) in pending.iter() {
        match &p.responder {
            Responder::Multi {
                deadline,
                responder,
                ..
            } => {
                if responder.is_closed() {
                    to_drop_single.push(*sid);
                } else if now >= *deadline && p.attempt > 0 {
                    // `attempt > 0` ensures the first broadcast went
                    // out — Reconnect entries waiting on a bucket fire
                    // would otherwise flush prematurely with empty
                    // results.
                    to_flush_multi.push(*sid);
                }
            }
            Responder::Single(tx) => {
                if tx.is_closed() {
                    to_drop_single.push(*sid);
                }
            }
        }
    }
    for sid in to_flush_multi {
        if let Some(p) = pending.remove(&sid) {
            by_name.remove(&p.pv_name);
            search_buckets[p.bucket].retain(|x| *x != sid);
            if let Responder::Multi {
                responder,
                accumulated,
                ..
            } = p.responder
            {
                let _ = responder.send(accumulated);
            }
        }
    }
    for sid in to_drop_single {
        if let Some(p) = pending.remove(&sid) {
            by_name.remove(&p.pv_name);
            search_buckets[p.bucket].retain(|x| *x != sid);
            // Sender drops at end of scope; that's the signal to the
            // caller (already-cancelled).
        }
    }
}

/// Returns bytes consumed from `bytes` so the caller can advance to
/// the next chained message in the same datagram (P-G10).
/// `is_tcp`: true when the response arrived on a TCP name-server connection;
/// enables pvxs procSearchReply port-0 rule (client.cpp:828-846).
fn handle_search_response(
    bytes: &[u8],
    pending: &mut HashMap<u32, Pending>,
    by_name: &mut HashMap<String, u32>,
    _beacons: &Arc<BeaconTracker>,
    ignore_guids: &std::collections::HashSet<[u8; 12]>,
    peer: SocketAddr,
    is_tcp: bool,
) -> usize {
    // Server-originated UDP — enforce direction bit (pvxs `conn.cpp:160`).
    let Ok(Some((frame, consumed))) = try_parse_frame_role(bytes, PeerRole::Client) else {
        return 0;
    };
    let Ok(resp) = decode_search_response(&frame) else {
        return consumed;
    };
    if !resp.found {
        return consumed;
    }
    // PVA-R10: pvxs `client.cpp:849-880` ignores SEARCH_RESPONSE
    // frames whose `proto != "tcp"`. The Rust UDP search engine
    // only opens plain TCP connections to resolved servers; a
    // server advertising a non-tcp transport (tls without an
    // established trust path, or an experimental scheme) must be
    // dropped — connecting on plain TCP would fail at handshake.
    // Accept "tcp" only here. An empty protocol is tolerated for
    // back-compat with older shims that omit the field.
    if !resp.protocol.is_empty() && resp.protocol != "tcp" {
        return consumed;
    }
    // pvxs procSearchReply (client.cpp:857-863) drops responses whose
    // server GUID is on the blocklist.
    if ignore_guids.contains(&resp.guid) {
        return consumed;
    }
    for cid in resp.cids {
        let mut server = rewrite_loopback(resp.server_addr, peer);
        // pvxs client.cpp:828-846: TCP NS with port=0 → use the NS connection port.
        // Covers the gateway self-serve case where the NS IS the data server.
        if is_tcp && server.port() == 0 {
            server.set_port(peer.port());
        }
        let Some(entry) = pending.get_mut(&cid) else {
            continue;
        };
        match &mut entry.responder {
            Responder::Single(_) => {
                // Single responder: deliver and remove.
                let p = pending.remove(&cid).unwrap();
                by_name.remove(&p.pv_name);
                if let Responder::Single(tx) = p.responder {
                    let _ = tx.send(server);
                }
            }
            Responder::Multi { accumulated, .. } => {
                if !accumulated.contains(&server) {
                    accumulated.push(server);
                }
                // Don't deliver yet — wait for the deadline tick to flush.
            }
        }
    }
    consumed
}

/// Returns bytes consumed from `bytes` so the caller can advance to
/// the next chained beacon in the same datagram (P-G10).
#[allow(clippy::too_many_arguments)]
fn handle_beacon(
    bytes: &[u8],
    beacons: &Arc<BeaconTracker>,
    pending: &mut HashMap<u32, Pending>,
    subscribers: &mut Vec<mpsc::Sender<Discovered>>,
    announced: &mut std::collections::HashSet<(SocketAddr, [u8; 12])>,
    poke_request: &mut bool,
    ignore_guids: &std::collections::HashSet<[u8; 12]>,
    peer: SocketAddr,
) -> usize {
    // Beacons are server-originated — enforce direction bit
    // (pvxs `conn.cpp:160`).
    let Ok(Some((frame, consumed))) = try_parse_frame_role(bytes, PeerRole::Client) else {
        return 0;
    };
    if frame.header.command != Command::Beacon.code() {
        return consumed;
    }
    let order = frame.header.flags.byte_order();
    let mut cur = Cursor::new(frame.payload.as_slice());
    let Ok(guid) = cur.get_bytes(12) else {
        return consumed;
    };
    // pvxs udp_collector.cpp::CMD_BEACON skips 4 bytes here:
    // flags(u8) + seq(u8) + change(u16). server.cpp::doBeacons emits
    // exactly this layout.
    let Ok(_flags) = cur.get_u8() else {
        return consumed;
    };
    let Ok(_seq) = cur.get_u8() else {
        return consumed;
    };
    let Ok(_change) = cur.get_u16(order) else {
        return consumed;
    };
    let Ok(addr_bytes) = cur.get_bytes(16) else {
        return consumed;
    };
    let Ok(port) = cur.get_u16(order) else {
        return consumed;
    };
    let proto = decode_string(&mut cur, order)
        .ok()
        .flatten()
        .unwrap_or_else(|| "tcp".into());
    let _status_size = decode_size(&mut cur, order).ok();

    let mut guid_arr = [0u8; 12];
    guid_arr.copy_from_slice(&guid);
    let mut addr_arr = [0u8; 16];
    addr_arr.copy_from_slice(&addr_bytes);
    // PVA-R18: accept all-zero (IPv6 unspecified) too — pvxs
    // `udp_collector.cpp:471-476` substitutes the UDP source for any
    // wildcard BEACON. Pre-fix Rust returned early and dropped the
    // beacon, so an IPv6-capable server advertising wildcard via the
    // raw-zero encoding never entered the tracker.
    let ip = ip_from_bytes_allow_unspec(&addr_arr);
    // pvxs udp_collector.cpp:480: when the beacon's advertised server
    // address is 0.0.0.0 (server bound wildcard), substitute the UDP
    // datagram's source address. Without this, we'd try to connect
    // back to 0.0.0.0:port — only valid loopback substitution catches
    // the same-host case.
    let resolved_ip = if ip.is_unspecified() { peer.ip() } else { ip };
    let server = SocketAddr::new(resolved_ip, port);

    if ignore_guids.contains(&guid_arr) {
        return consumed;
    }
    let allow_reconnect = beacons.observe(server, guid_arr);
    let first_announce = announced.insert((server, guid_arr));
    // pvxs poke() — only kick on FRESH server identity (mirror of the
    // SearchCommand::BeaconObserved path). A long-running server's
    // periodic beacons should not constantly bring pending searches'
    // retry deadlines forward. Set the poke_request flag so the main
    // loop can also flip the tick cadence to fast (200 ms × 30) for
    // one revolution.
    if allow_reconnect && first_announce {
        *poke_request = true;
        for p in pending.values_mut() {
            p.last_attempt = Instant::now() - Duration::from_secs(60);
            // NOTE: more aggressive than pvxs's `poked` semantic
            // (which preserves nSearch and skips the increment for
            // one tick). See the BeaconObserved arm in `run_engine`
            // for the full rationale. Acceptable trade for single-
            // channel recovery.
            p.attempt = 0;
        }
    }
    if first_announce {
        let evt = Discovered::Online {
            server,
            guid: guid_arr,
            peer,
            proto,
        };
        subscribers.retain(|tx| tx.try_send(evt.clone()).is_ok());
    }
    consumed
}

fn rewrite_loopback(addr: SocketAddr, peer: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() || addr.ip().is_loopback() {
        if !peer.ip().is_loopback() {
            SocketAddr::new(peer.ip(), addr.port())
        } else {
            // PR #205 IPv6 Stage 4: when both ends are loopback, mirror
            // the peer's family rather than hard-coding `127.0.0.1`.
            // For a v6 SEARCH that arrived from `[::1]` the resolved
            // address must stay on `[::1]` so the subsequent TCP
            // connect targets the v6 listener.
            let lo = match peer.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            };
            SocketAddr::new(lo, addr.port())
        }
    } else {
        addr
    }
}

// ── TCP name-server tasks ───────────────────────────────────────────────

/// Long-running task for one EPICS_PVA_NAME_SERVERS entry.
/// Loops forever: connect, handshake, forward SEARCHes / receive responses,
/// reconnect after 10 s on any failure. pvxs client.cpp:651-667 + 1295-1305.
async fn ns_task(
    ns_addr: SocketAddr,
    mut search_rx: mpsc::Receiver<Vec<u8>>,
    response_tx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
) {
    // pvxs client.cpp:68: tcpNSCheckInterval = 10s.
    const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
    loop {
        if let Err(e) = ns_run_once(ns_addr, &mut search_rx, &response_tx).await {
            debug!(
                target: "epics_pva_rs::client",
                "NS {ns_addr} disconnected: {e}; reconnecting in {RECONNECT_INTERVAL:?}"
            );
        }
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

/// One connection attempt to `ns_addr`: TCP connect → PVA handshake → forward
/// SEARCH frames and route SEARCH_RESPONSE bytes back to the engine.
async fn ns_run_once(
    ns_addr: SocketAddr,
    search_rx: &mut mpsc::Receiver<Vec<u8>>,
    response_tx: &mpsc::Sender<(Vec<u8>, SocketAddr)>,
) -> std::io::Result<()> {
    let stream = TcpStream::connect(ns_addr).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let mut rx_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // ── Handshake: wait for SET_BYTE_ORDER + CONNECTION_VALIDATION request ──
    let mut byte_order = ByteOrder::Little;
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "NS closed during handshake",
            ));
        }
        rx_buf.extend_from_slice(&tmp[..n]);
        let mut pos = 0;
        let mut got_validation_req = false;
        while rx_buf.len().saturating_sub(pos) >= PvaHeader::SIZE {
            let Ok(hdr) = PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[pos..])) else {
                break;
            };
            let frame_end = pos + PvaHeader::SIZE + hdr.payload_length as usize;
            if rx_buf.len() < frame_end {
                break;
            }
            if hdr.flags.is_control() {
                if hdr.command == ControlCommand::SetByteOrder.code() {
                    byte_order = hdr.flags.byte_order();
                }
            } else if hdr.command == Command::ConnectionValidation.code() {
                got_validation_req = true;
            }
            pos = frame_end;
        }
        rx_buf.drain(..pos);
        if got_validation_req {
            break;
        }
    }

    // ── Send anonymous CONNECTION_VALIDATION reply ───────────────────────────
    // pvxs clientconn.cpp:163-174: NS only needs SEARCH routing, not channel
    // ops — anonymous auth is sufficient.
    writer
        .write_all(&build_ns_connection_validation(byte_order))
        .await?;

    // ── Wait for CONNECTION_VALIDATED ────────────────────────────────────────
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "NS closed before validated",
            ));
        }
        rx_buf.extend_from_slice(&tmp[..n]);
        let mut pos = 0;
        let mut validated = false;
        while rx_buf.len().saturating_sub(pos) >= PvaHeader::SIZE {
            let Ok(hdr) = PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[pos..])) else {
                break;
            };
            let frame_end = pos + PvaHeader::SIZE + hdr.payload_length as usize;
            if rx_buf.len() < frame_end {
                break;
            }
            if !hdr.flags.is_control() && hdr.command == Command::ConnectionValidated.code() {
                validated = true;
            }
            pos = frame_end;
        }
        rx_buf.drain(..pos);
        if validated {
            break;
        }
    }

    // ── Main loop: forward SEARCH frames out; route SEARCH_RESPONSE back ────
    loop {
        tokio::select! {
            pkt = search_rx.recv() => {
                match pkt {
                    Some(bytes) => writer.write_all(&bytes).await?,
                    // Engine dropped the sender — shut down cleanly.
                    None => return Ok(()),
                }
            }
            res = reader.read(&mut tmp) => {
                let n = res?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "NS closed",
                    ));
                }
                rx_buf.extend_from_slice(&tmp[..n]);
                let mut pos = 0;
                while rx_buf.len().saturating_sub(pos) >= PvaHeader::SIZE {
                    let Ok(hdr) =
                        PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[pos..]))
                    else {
                        break;
                    };
                    let frame_end = pos + PvaHeader::SIZE + hdr.payload_length as usize;
                    if rx_buf.len() < frame_end {
                        break;
                    }
                    if !hdr.flags.is_control()
                        && hdr.command == Command::SearchResponse.code()
                    {
                        let frame_bytes = rx_buf[pos..frame_end].to_vec();
                        let _ = response_tx.try_send((frame_bytes, ns_addr));
                    }
                    pos = frame_end;
                }
                rx_buf.drain(..pos);
            }
        }
    }
}

/// Minimal CONNECTION_VALIDATION payload for anonymous NS connections.
/// pvxs clientconn.cpp:163-174: buffer_size=1MiB, registry=0x7FFF, QOS=0,
/// auth="anonymous", null variant (0xFF).
fn build_ns_connection_validation(order: ByteOrder) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(1024 * 1024, order);
    payload.put_u16(0x7FFF, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.push(0xFF); // null variant
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

#[allow(dead_code)]
fn _suppress(_: PvaHeader) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_last_value() {
        let max = *BACKOFF_SECS.last().unwrap();
        for i in 0..50 {
            let v = BACKOFF_SECS[i.min(BACKOFF_SECS.len() - 1)];
            assert!(v <= max);
        }
    }

    #[test]
    fn rewrite_loopback_preserves_real_addr() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5076);
        assert_eq!(rewrite_loopback(a, peer), a);
    }

    #[test]
    fn rewrite_loopback_unspecified_uses_remote_peer() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(
            r,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5075)
        );
    }

    #[test]
    fn rewrite_loopback_unspecified_with_loopback_peer_falls_back_to_localhost() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(r, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075));
    }

    // Extends pvxs `procSearchReply`, which only rewrites `isAny()`. A
    // pvAccessCPP server bound via `EPICS_PVAS_INTF_ADDR_LIST=127.0.0.1`
    // emits a literal 127.0.0.1 in its search reply; treat the wire
    // address as unreliable when the UDP packet came from a non-loopback
    // peer.
    #[test]
    fn rewrite_loopback_explicit_loopback_overridden_by_remote_peer() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(
            r,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5075)
        );
    }

    #[test]
    fn rewrite_loopback_explicit_loopback_kept_for_loopback_peer() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(r, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075));
    }

    /// pvxs `Channel::disconnect` (client.cpp:213) parity: a
    /// `Reconnect` SEARCH lands in the CURRENT bucket — not a
    /// sid-hashed slot. Cascade-spread on first reconnect is
    /// achieved naturally by the one-bucket-per-tick rate-limit;
    /// the per-channel `nSearch`-bucket forward push handles
    /// retry escalation. See decision
    /// `bc7a1203-cac5-4ba1-a5b6-194e1a564482`.
    #[test]
    fn placement_reconnect_uses_current_bucket() {
        for current in 0..N_SEARCH_BUCKETS {
            assert_eq!(
                placement_bucket(current, SearchReason::Reconnect),
                current,
                "Reconnect must drop in current bucket (got {current})"
            );
        }
    }

    /// `Initial` is paired with an immediate broadcast in the Find /
    /// FindAll handlers, so its bucket placement is one tick ahead
    /// — that's where the FIRST scheduled retransmit (after the
    /// immediate fire) lands. Wrap-around at the ring boundary is
    /// part of the contract and exercised here.
    #[test]
    fn placement_initial_is_one_bucket_ahead_with_wraparound() {
        assert_eq!(placement_bucket(0, SearchReason::Initial), 1);
        assert_eq!(placement_bucket(13, SearchReason::Initial), 14);
        assert_eq!(
            placement_bucket(N_SEARCH_BUCKETS - 1, SearchReason::Initial),
            0,
            "wrap-around at ring boundary"
        );
    }

    /// pvxs `tickSearch` (client.cpp:1193-1196) escalates the retry
    /// bucket by `nSearch+1` after each transmit. We verify the
    /// pattern bucket-by-bucket: 1, 2, 3, 4, … capping at
    /// `N_SEARCH_BUCKETS`. Wrap-around plays into the cap behaviour
    /// because we don't go past the ring; once `attempt` hits
    /// `N_SEARCH_BUCKETS`, the increment lands on the SAME index
    /// (full ring), which is the steady-state 30-tick retry cadence.
    #[test]
    fn cascade_next_implements_pvxs_nsearch_escalation() {
        let no_imbalance = |_| 0usize;
        let current = 7;

        // attempt=1: idx + 1
        assert_eq!(
            cascade_smoothed_next(current, 1, no_imbalance),
            (current + 1) % N_SEARCH_BUCKETS,
        );
        // attempt=2: idx + 2
        assert_eq!(
            cascade_smoothed_next(current, 2, no_imbalance),
            (current + 2) % N_SEARCH_BUCKETS,
        );
        // attempt=10: idx + 10
        assert_eq!(
            cascade_smoothed_next(current, 10, no_imbalance),
            (current + 10) % N_SEARCH_BUCKETS,
        );
        // attempt=N_SEARCH_BUCKETS: wraps to current (full ring).
        assert_eq!(
            cascade_smoothed_next(current, N_SEARCH_BUCKETS as u32, no_imbalance),
            current,
        );
        // attempt > N_SEARCH_BUCKETS: still capped, same behaviour.
        assert_eq!(
            cascade_smoothed_next(current, 1_000_000, no_imbalance),
            current,
        );
    }

    /// pvxs `client.cpp:1199-1206` smoothing: when the chosen `next`
    /// bucket is overloaded versus `next+1` by 100+ entries, defer
    /// to `next+1`. Crosses two ticks instead of one, evening out
    /// the burst. Below the 100-entry delta the smoothing must NOT
    /// trigger — even moderate imbalance is acceptable in exchange
    /// for keeping the retry latency tight.
    #[test]
    fn cascade_smoothing_defers_when_next_is_overloaded() {
        let current = 5;
        let attempt = 1; // → next = 6, nextnext = 7

        // 200 entries at 6, 0 at 7: defer → 7.
        let sizes_overloaded = |idx: usize| match idx {
            6 => 200,
            _ => 0,
        };
        assert_eq!(
            cascade_smoothed_next(current, attempt, sizes_overloaded),
            7,
            "imbalance > 100 must defer to nextnext"
        );

        // 90 entries at 6, 0 at 7: NOT enough to trigger smoothing.
        let sizes_below_threshold = |idx: usize| match idx {
            6 => 90,
            _ => 0,
        };
        assert_eq!(
            cascade_smoothed_next(current, attempt, sizes_below_threshold),
            6,
            "imbalance ≤ 100 stays in next"
        );

        // 200 at next AND 200 at nextnext: no asymmetry → stay in next.
        let sizes_balanced = |idx: usize| match idx {
            6 | 7 => 200,
            _ => 0,
        };
        assert_eq!(cascade_smoothed_next(current, attempt, sizes_balanced), 6,);

        // Overload at nextnext (not next): smoothing only defers
        // FORWARD, never backward — stays in next.
        let sizes_reverse_overload = |idx: usize| match idx {
            7 => 200,
            _ => 0,
        };
        assert_eq!(
            cascade_smoothed_next(current, attempt, sizes_reverse_overload),
            6,
        );
    }

    /// `find_all` must NOT hang indefinitely when no server claims the
    /// PV — review finding #3. With the fix, the tick handler flushes
    /// the Multi responder at deadline even with empty `accumulated`,
    /// so the user-visible future resolves to `Vec::new()` and any
    /// outer `PvaClient::timeout` gets a chance to apply.
    #[tokio::test(flavor = "current_thread")]
    async fn find_all_returns_empty_when_no_responder() {
        // Suppress UDP fan-out — we don't want the engine bound to
        // 5076 in CI / racing with a real PVA server.
        //
        // SAFETY: env vars are process-global; this test is annotated
        // current_thread so other tokio tests don't see a partial
        // state. The variables aren't read by any production path
        // running in the same process.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }

        let engine = SearchEngine::spawn(Vec::new(), Vec::new())
            .await
            .expect("spawn engine");

        let started = std::time::Instant::now();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            engine.find_all("MISSING:PV", SearchReason::Initial),
        )
        .await
        .expect("find_all must complete (not hang) — review finding #3")
        .expect("find_all should not error");
        let elapsed = started.elapsed();

        // With Initial: immediate broadcast, deadline at find_all
        // time + MULTI_SERVER_WINDOW (200 ms). The next 1-s tick
        // boundary after deadline triggers the empty-flush path.
        assert!(res.is_empty(), "no servers should be discovered");
        assert!(
            elapsed < Duration::from_secs(3),
            "must flush within a few ticks; took {elapsed:?}"
        );
    }

    /// End-to-end Reconnect bucket-fire test. Boots a real
    /// `SearchEngine`, binds a sniffer socket on localhost as the
    /// only broadcast destination, submits a `Find(Reconnect)`, and
    /// asserts that a SEARCH packet for the right PV name lands on
    /// the sniffer within ~1.1 s — i.e. the next tick after `Find`
    /// arrival, mirroring pvxs `Channel::disconnect` recovery
    /// timing. Without the v0.13.x fix the search would have been
    /// placed in a sid-hashed bucket up to 30 s away and never
    /// fired (the channel-layer timeout would have cancelled the
    /// caller's oneshot before any tick processed it).
    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_search_broadcasts_within_one_tick() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        // Suppress real broadcast targets so the only destination
        // is our sniffer below. SAFETY: see find_all_returns_empty.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }

        // Sniffer on loopback ephemeral. The engine will be told
        // about it via `extra_targets`.
        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer local_addr");

        let engine = SearchEngine::spawn(vec![sniffer_addr], Vec::new())
            .await
            .expect("spawn engine");

        // Issue a Reconnect find. The engine places it in
        // `current_bucket`; the next 1-Hz tick fires the broadcast.
        // We poll the sniffer until a packet shows up. Cap the
        // wait at 3 s — pvxs-equivalent timing is ≤ 1.1 s; the
        // extra ~2 s is jitter slack so the test isn't flaky on a
        // loaded CI runner.
        let pv = "TEST:RECONNECT:PV";
        let started = std::time::Instant::now();
        let find_handle = tokio::spawn({
            let engine = engine.clone();
            let pv = pv.to_string();
            async move { engine.find(&pv, SearchReason::Reconnect).await }
        });

        let mut buf = vec![0u8; 4096];
        let recv_result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (n, _from) = sniffer.recv_from(&mut buf).await?;
                // The CMD_SEARCH frame includes the PV name as a
                // C-string somewhere in the payload. We don't need
                // a full decoder for this test — substring match
                // is conclusive enough since random ephemeral UDP
                // traffic on loopback isn't going to spell out
                // our chosen PV name by accident.
                if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    return Ok::<usize, std::io::Error>(n);
                }
                // Not our packet (unlikely on isolated loopback
                // sniffer, but defensive). Loop back and read
                // again.
            }
        })
        .await;

        let elapsed = started.elapsed();
        find_handle.abort();

        let n = recv_result
            .expect("Reconnect SEARCH must arrive within 3 s")
            .expect("recv_from must not error");
        assert!(
            n > 0,
            "received an empty datagram — Reconnect SEARCH path is broken"
        );
        // Tight assertion catches the regression we're guarding
        // against (5-30 s pre-fix latency) without being flaky on a
        // loaded CI runner. 2.5 s gives ~1.5 s of slack on top of
        // the ≤ 1.1 s pvxs-parity target.
        assert!(
            elapsed < Duration::from_millis(2500),
            "Reconnect should broadcast within ~1.1 s (one tick); \
             took {elapsed:?} — bucket placement / tick handler may \
             have regressed (review decision \
             bc7a1203-cac5-4ba1-a5b6-194e1a564482)"
        );
    }

    /// Regression guard for the `RECONNECT_FIND_TIMEOUT` removal:
    /// the engine's `find(Reconnect)` future must NOT resolve early
    /// when no server is responding. It must stay pending until
    /// either (a) a SEARCH_RESPONSE arrives, (b) the caller drops
    /// the future. Without this guard a future PR could
    /// reintroduce a caller-side timeout (or change `find_all`'s
    /// empty-deadline behaviour to fire for `find` too) and
    /// silently revive the disconnect-storm bug.
    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_find_does_not_complete_without_response() {
        // Suppress real broadcast so no actual SEARCH leaves the
        // process to potentially get answered by some other PVA
        // server on the LAN.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }
        let engine = SearchEngine::spawn(Vec::new(), Vec::new())
            .await
            .expect("spawn engine");

        // Race the find against a 1.5 s sleep. The bucket fire is
        // expected at ~1 s (one tick after the Find lands), but no
        // server replies, so the find() future should still be
        // pending after the bucket fire. 1.5 s > 1.1 s ensures the
        // bucket has fired (so we know the test isn't passing for
        // the wrong reason — i.e. the engine never even got to
        // process the Find). After the sleep wins, we drop the
        // find future.
        let timed_out = tokio::select! {
            biased;
            _ = engine.find("MISSING:RECONNECT:PV", SearchReason::Reconnect) => false,
            _ = tokio::time::sleep(Duration::from_millis(1500)) => true,
        };
        assert!(
            timed_out,
            "Reconnect find() resolved early (no server is responding); \
             a caller-side timeout has been reintroduced — see \
             channel.rs::ensure_active. find() must stay pending until \
             SEARCH_RESPONSE arrives or the caller drops the future."
        );
    }

    /// Smoothing boundary cases — pvxs's threshold is strictly
    /// `delta > 100`. Tests one entry below (delta=100, stays in
    /// `next`) and one above (delta=101, defers to `nextnext`).
    /// Catches the easy-to-introduce off-by-one (`>= 100`).
    #[test]
    fn cascade_smoothing_boundary_at_delta_100() {
        let current = 5;
        let attempt = 1; // → next = 6, nextnext = 7

        // delta = 100 (100 vs 0): NOT enough to trigger.
        let exactly_100 = |idx: usize| match idx {
            6 => 100,
            _ => 0,
        };
        assert_eq!(
            cascade_smoothed_next(current, attempt, exactly_100),
            6,
            "delta == 100 must NOT trigger smoothing (strict > 100)"
        );

        // delta = 101: triggers.
        let just_over_100 = |idx: usize| match idx {
            6 => 101,
            _ => 0,
        };
        assert_eq!(
            cascade_smoothed_next(current, attempt, just_over_100),
            7,
            "delta == 101 must trigger smoothing"
        );
    }

    /// `HurryUp` (and equivalently `BeaconObserved` for fresh GUID)
    /// must:
    ///   1. Switch the engine into 200 ms fast-tick cadence.
    ///   2. Reset all pending searches' attempt counters so the
    ///      kicked retries cascade from the 1-bucket forward push.
    ///   3. Make the next SEARCH for an existing pending fire
    ///      within ~250 ms (one fast tick), not the next 1 s tick.
    ///
    /// We observe the BEHAVIOUR (SEARCH packet timing on a sniffer),
    /// not internal state, because internal fields aren't part of
    /// the engine's public contract.
    #[tokio::test(flavor = "current_thread")]
    async fn hurry_up_kicks_pending_searches_at_fast_tick_cadence() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }
        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer local_addr");

        let engine = SearchEngine::spawn(vec![sniffer_addr], Vec::new())
            .await
            .expect("spawn engine");
        let pv = "TEST:HURRYUP:PV";

        // Submit Reconnect find — placed at current_bucket, will
        // fire at next 1 s tick.
        let _find_handle = tokio::spawn({
            let engine = engine.clone();
            let pv = pv.to_string();
            async move { engine.find(&pv, SearchReason::Reconnect).await }
        });

        // Drain the FIRST SEARCH (the one fired by the normal
        // 1-Hz tick after Find arrives). This puts the search
        // into `attempt=1` state, parked in some retry bucket.
        let mut buf = vec![0u8; 4096];
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (n, _) = sniffer.recv_from(&mut buf).await.expect("first recv");
                if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    return;
                }
            }
        })
        .await
        .expect("first SEARCH must arrive");

        // Now hit HurryUp. This switches to 200 ms cadence and
        // resets attempt — the kicked retry should fire on the
        // FAST tick, not the slow 1 s tick.
        engine.hurry_up().await;
        let started = std::time::Instant::now();

        // Look for the SECOND SEARCH packet for the same PV.
        // Bound at 1 s — much shorter than the slow 1 s tick that
        // would be next without fast-tick mode. (We just consumed
        // the slow tick's broadcast above; the next slow tick is
        // ~1 s away. If HurryUp doesn't activate fast-tick, this
        // test fails.)
        let elapsed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let (n, _) = sniffer.recv_from(&mut buf).await.expect("second recv");
                if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    return started.elapsed();
                }
            }
        })
        .await
        .expect("HurryUp-kicked SEARCH must arrive within 1 s — fast-tick mode regressed");

        // Pre-fix slow tick would have given ~1 s here. Fast tick
        // (200 ms) plus jitter is well under 700 ms even on a
        // loaded CI runner.
        assert!(
            elapsed < Duration::from_millis(700),
            "HurryUp should fire next SEARCH within one fast tick (~200 ms); \
             took {elapsed:?} — fast-tick mode may have regressed"
        );
    }

    /// End-to-end retry escalation timing test. Verifies that the
    /// production engine loop reproduces pvxs's `nSearch+1`
    /// pattern at the actual scheduler level — unit tests of
    /// `cascade_smoothed_next` cover the formula in isolation, but
    /// only this test catches an accumulator drift between the
    /// pure fn and the live `current_bucket`-advancing tick loop.
    ///
    /// Expected SEARCH arrival times (relative to find submission):
    ///   #1 at ~1 s   (first tick after Find lands)
    ///   #2 at ~2 s   (idx+1, +1 cycle)
    ///   #3 at ~4 s   (idx+(1+2)=idx+3, +2 cycles)
    ///
    /// Slack: ±500 ms per gap to absorb scheduler / mio jitter on
    /// loaded CI. Total runtime ~4 s.
    #[tokio::test(flavor = "current_thread")]
    async fn retry_escalation_pvxs_pattern() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }
        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer addr");
        let engine = SearchEngine::spawn(vec![sniffer_addr], Vec::new())
            .await
            .expect("spawn");

        let pv = "ESCALATION:PVA";
        let started = std::time::Instant::now();
        let _find_handle = tokio::spawn({
            let engine = engine.clone();
            let pv = pv.to_string();
            async move { engine.find(&pv, SearchReason::Reconnect).await }
        });

        let mut buf = vec![0u8; 4096];
        let mut packet_times = Vec::new();
        for i in 0..3 {
            let t = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let (n, _) = sniffer.recv_from(&mut buf).await.expect("recv");
                    if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                        return started.elapsed();
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("SEARCH #{} did not arrive within 8 s", i + 1));
            packet_times.push(t);
        }

        // Gap assertions. T#1 at ~1 s; T#2 at ~T#1+1 s; T#3 at ~T#2+2 s.
        assert!(
            packet_times[0] < Duration::from_millis(1500),
            "first SEARCH should arrive ~1 s after Find; got {:?}",
            packet_times[0]
        );
        let gap_12 = packet_times[1].saturating_sub(packet_times[0]);
        let gap_23 = packet_times[2].saturating_sub(packet_times[1]);
        assert!(
            (700..=1500).contains(&(gap_12.as_millis() as u64)),
            "gap #1→#2 should be ~1 s (nSearch=1); got {gap_12:?}. \
             Production retry escalation may have regressed."
        );
        assert!(
            (1500..=2700).contains(&(gap_23.as_millis() as u64)),
            "gap #2→#3 should be ~2 s (nSearch=2); got {gap_23:?}. \
             Production retry escalation may have regressed."
        );
    }

    /// PR #205 IPv6 Stage 4: client SEARCH must reach a v6 server.
    /// Sets up a v6-bound PVA server (TCP via Stage 1, UDP via Stage
    /// 2), spawns a SearchEngine with the server's v6 UDP address in
    /// `extra_targets`, and verifies `find()` resolves to the server's
    /// TCP endpoint. Regression guard against the v6 send path being
    /// dropped or the v6 recv arm losing the SEARCH_RESPONSE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn client_search_resolves_over_ipv6() {
        use crate::nt::typed::TypedNT;
        use crate::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};
        use std::net::Ipv6Addr;

        // Suppress NIC broadcast so accidental v4 traffic to a sibling
        // pva-rs server on this host can't resolve the PV name.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }

        let pv = SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&2.5));
        let source = Arc::new(SharedSource::new());
        source.add("V6:SEARCH:PV", pv);

        // Bind UDP on an OS-picked port so the v6 responder can claim
        // the same port via its `[::]:udp_port` listener.
        let pick_udp = || {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .expect("udp probe bind");
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let udp_port = pick_udp();

        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port,
            bind_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            enable_ipv6_udp: true,
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("test server must start");
        let server_tcp_port = server.report().tcp_port;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            server.report().udp_v6_alive,
            "udp_v6 responder must be alive"
        );

        let v6_target = SocketAddr::V6(std::net::SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            udp_port,
            0,
            0,
        ));
        let engine = SearchEngine::spawn(vec![v6_target], Vec::new())
            .await
            .expect("spawn engine");

        let resolved = tokio::time::timeout(
            Duration::from_secs(5),
            engine.find("V6:SEARCH:PV", SearchReason::Initial),
        )
        .await
        .expect("SEARCH over IPv6 timed out")
        .expect("SEARCH must resolve over IPv6");

        assert_eq!(
            resolved.port(),
            server_tcp_port,
            "resolved TCP port must match v6 server's listener"
        );
        assert!(
            matches!(resolved.ip(), IpAddr::V6(_)),
            "resolved server address must be IPv6; got {resolved:?}"
        );
        drop(server);
    }

    /// PR #205 IPv6 Stage 6: `bind_beacon_udp_v6` returns a usable
    /// socket bound to `[::]:5076` (or `EPICS_PVA_BROADCAST_PORT`)
    /// with the default v6 multicast group joined. Confirms the
    /// plumbing — without it the recv arm in `run_engine` never
    /// fires for v6 beacons.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v6_beacon_socket_binds_and_joins_default_group() {
        // Use a unique broadcast port for this test so we don't fight
        // a parallel test or a local pva-rs IOC for the well-known
        // 5076 socket. Picks via OS-coordinated probe, drops, then
        // env-sets so `bind_beacon_udp_v6` reads the same port.
        let pick_port = || {
            let s = std::net::UdpSocket::bind("[::1]:0").expect("v6 probe bind");
            let p = s.local_addr().unwrap().port();
            drop(s);
            p
        };
        let port = pick_port();
        // SAFETY: test process-wide env mutation. Tests reading the
        // same var serialise on this section via tokio's runtime
        // ordering — set + bind are sequential.
        unsafe {
            std::env::set_var("EPICS_PVA_BROADCAST_PORT", port.to_string());
        }

        let sock =
            bind_beacon_udp_v6().expect("v6 beacon socket must bind on a host with IPv6 enabled");
        let local = sock.local_addr().expect("local_addr");
        assert!(
            matches!(local.ip(), IpAddr::V6(_)),
            "beacon socket must be IPv6; got {local}"
        );
        assert_eq!(
            local.port(),
            port,
            "beacon socket must bind the EPICS_PVA_BROADCAST_PORT we set"
        );

        // Best-effort cleanup so a re-run on the same port doesn't
        // inherit a stale value.
        unsafe {
            std::env::remove_var("EPICS_PVA_BROADCAST_PORT");
        }
        drop(sock);
    }

    /// PR #205 IPv6 Stage 6 end-to-end: spawn a SearchEngine and emit
    /// a synthetic v6 beacon to its v6 beacon socket. The engine's
    /// recv arm decodes the beacon, the BeaconTracker observes the
    /// (server_addr, guid) pair, and `beacon_guid_for(addr)` returns
    /// the GUID we sent. Guards the full recv-decode-track chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v6_beacon_arriving_at_engine_is_observed_by_tracker() {
        use crate::proto::{ByteOrder, Command, PvaHeader, WriteExt};

        // Pick a free v6 UDP port. The engine's v6 beacon socket and
        // our sender both target this port.
        let pick_port = || {
            let s = std::net::UdpSocket::bind("[::1]:0").expect("probe");
            let p = s.local_addr().unwrap().port();
            drop(s);
            p
        };
        let port = pick_port();
        // SAFETY: process-wide env mutation. Suppress v4 search
        // destinations so the engine's broadcast loop has nothing
        // useful to fire.
        unsafe {
            std::env::set_var("EPICS_PVA_BROADCAST_PORT", port.to_string());
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }

        let engine = SearchEngine::spawn(Vec::new(), Vec::new())
            .await
            .expect("spawn engine");
        // Give the engine a moment to bind sockets and enter its
        // select loop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Hand-roll a beacon frame: PVA header + 12B GUID + flags +
        // seq + change_count + 16B server addr (IPv4-mapped ::FFFF:0)
        // + tcp_port + protocol("tcp") + 0xFF status marker.
        let guid: [u8; 12] = [0x42; 12];
        let tcp_port: u16 = 5099;
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.extend_from_slice(&guid);
        payload.put_u8(0); // flags
        payload.put_u8(7); // seq
        payload.put_u16(0, order); // change_count
        // 16-byte server addr = ::FFFF:0.0.0.0 (unspecified)
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0, 0]);
        payload.put_u16(tcp_port, order);
        // protocol string "tcp" (size-prefixed)
        crate::proto::encode_string_into("tcp", order, &mut payload);
        payload.put_u8(0xFF); // null serverStatus marker
        let header =
            PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
        let mut frame = Vec::new();
        header.write_into(&mut frame);
        frame.extend_from_slice(&payload);

        // Send the beacon from an ephemeral v6 socket to the
        // engine's listener on `[::1]:port`.
        let tx = tokio::net::UdpSocket::bind("[::1]:0")
            .await
            .expect("tx bind");
        let dest = SocketAddr::V6(std::net::SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0));
        tx.send_to(&frame, dest).await.expect("send beacon");

        // Poll the tracker for up to ~2s — the engine processes the
        // beacon asynchronously. The tracker keys on the resolved
        // server addr (peer IP + advertised tcp_port). For an
        // unspecified wire address with a loopback peer, rewriter
        // logic resolves to `[::1]:tcp_port`.
        let resolved_server = SocketAddr::V6(std::net::SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            tcp_port,
            0,
            0,
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let found_guid = loop {
            if let Some(g) = engine.beacon_guid_for(resolved_server) {
                break Some(g);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        // Cleanup env before assertion so a failure doesn't leave it
        // set for sibling tests.
        unsafe {
            std::env::remove_var("EPICS_PVA_BROADCAST_PORT");
            std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST");
            std::env::remove_var("EPICS_PVA_ADDR_LIST");
        }

        let observed = found_guid.expect(
            "BeaconTracker must observe a beacon arriving on the v6 beacon socket; \
             v6 recv arm in run_engine may be broken",
        );
        assert_eq!(observed, guid, "tracker must record the exact GUID");
    }

    /// PVA-R4 regression: TCP name servers must resolve PVs via persistent
    /// SEARCH/SEARCH_RESPONSE, not merely as direct-connect fallbacks.
    ///
    /// Spawns a mock TCP name-server that performs the full PVA handshake
    /// (SET_BYTE_ORDER → CONNECTION_VALIDATION → CONNECTION_VALIDATED) then
    /// replies to any SEARCH frame with a SEARCH_RESPONSE containing the
    /// matching search_id. With EPICS_PVA_AUTO_ADDR_LIST=NO the only
    /// resolution path is the TCP NS — pre-fix find() would time out because
    /// the NS was never wired into the search path; post-fix it resolves
    /// within the timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pva_r4_tcp_nameserver_persistent_peer() {
        use std::io::Cursor as IoCursor;
        use tokio::net::TcpListener;

        // Bind mock NS before spawning engine so the port is known.
        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock NS listener bind");
        let ns_addr = ns_listener.local_addr().unwrap();

        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }

        let ns_handle = tokio::spawn(async move {
            let (mut stream, _peer) = ns_listener.accept().await.expect("mock NS: accept");
            let order = ByteOrder::Little;

            // ── Step 1: SET_BYTE_ORDER ─────────────────────────────────────────
            {
                let mut buf = Vec::with_capacity(PvaHeader::SIZE);
                PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0)
                    .write_into(&mut buf);
                stream.write_all(&buf).await.expect("mock NS: write SBO");
            }

            // ── Step 2: CONNECTION_VALIDATION request (server→client) ──────────
            {
                let mut payload = Vec::new();
                payload.put_u32(87_040, order); // buffer_size
                payload.put_u16(32_767, order); // registry_size
                payload.push(1u8); // auth_methods count (size-encoded, 1 < 254)
                encode_string_into("anonymous", order, &mut payload);
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::ConnectionValidation.code(),
                    payload.len() as u32,
                );
                let mut frame = Vec::new();
                h.write_into(&mut frame);
                frame.extend_from_slice(&payload);
                stream
                    .write_all(&frame)
                    .await
                    .expect("mock NS: write val req");
            }

            // ── Step 3: drain client CONNECTION_VALIDATION reply ───────────────
            {
                let mut buf = Vec::<u8>::new();
                let mut tmp = [0u8; 4096];
                'drain_val: loop {
                    let n = stream
                        .read(&mut tmp)
                        .await
                        .expect("mock NS: read val reply");
                    assert!(n > 0, "mock NS: client closed before validation");
                    buf.extend_from_slice(&tmp[..n]);
                    let mut pos = 0usize;
                    while buf.len().saturating_sub(pos) >= PvaHeader::SIZE {
                        let Ok(hdr) = PvaHeader::decode(&mut IoCursor::new(&buf[pos..])) else {
                            break;
                        };
                        let frame_end = pos + PvaHeader::SIZE + hdr.payload_length as usize;
                        if buf.len() < frame_end {
                            break;
                        }
                        if !hdr.flags.is_control()
                            && hdr.command == Command::ConnectionValidation.code()
                        {
                            break 'drain_val;
                        }
                        pos = frame_end;
                    }
                }
            }

            // ── Step 4: CONNECTION_VALIDATED ───────────────────────────────────
            {
                // Status::OkNoMsg wire encoding = 0xFF (proto/status.rs:143-144).
                let payload = vec![0xFFu8];
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::ConnectionValidated.code(),
                    payload.len() as u32,
                );
                let mut frame = Vec::new();
                h.write_into(&mut frame);
                frame.extend_from_slice(&payload);
                stream
                    .write_all(&frame)
                    .await
                    .expect("mock NS: write validated");
            }

            // ── Step 5: read SEARCH frames, reply with SEARCH_RESPONSE ─────────
            // SEARCH payload offsets (codec.rs:88-102, LE byte order):
            //   seq(4) + flags(1) + reserved(3) + addr(16) + port(2)
            //   + proto_count(1) + "tcp"(1+3) + ch_count(2) = 33 bytes
            //   → search_id at payload[33..37].
            let mut buf = Vec::<u8>::new();
            let mut tmp = [0u8; 4096];
            let mut responded = false;
            loop {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let mut pos = 0usize;
                while buf.len().saturating_sub(pos) >= PvaHeader::SIZE {
                    let Ok(hdr) = PvaHeader::decode(&mut IoCursor::new(&buf[pos..])) else {
                        break;
                    };
                    let frame_end = pos + PvaHeader::SIZE + hdr.payload_length as usize;
                    if buf.len() < frame_end {
                        break;
                    }
                    if !hdr.flags.is_control()
                        && hdr.command == Command::Search.code()
                        && !responded
                    {
                        let pl = &buf[pos + PvaHeader::SIZE..frame_end];
                        if pl.len() >= 37 {
                            let search_id = u32::from_le_bytes(pl[33..37].try_into().unwrap());
                            let guid = [0x42u8; 12];
                            let resp = crate::server_native::udp::build_search_response_proto(
                                guid,
                                0,
                                ns_addr.port(),
                                &[search_id],
                                order,
                                "tcp",
                            );
                            stream
                                .write_all(&resp)
                                .await
                                .expect("mock NS: write SEARCH_RESPONSE");
                            responded = true;
                        }
                    }
                    pos = frame_end;
                }
                buf.drain(..pos);
            }
        });

        // Engine with only this NS — no UDP broadcast.
        let engine = SearchEngine::spawn(Vec::new(), vec![ns_addr])
            .await
            .expect("spawn engine");

        // find() must resolve via the TCP NS within 5 s.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            engine.find("TEST:NS:PV", SearchReason::Initial),
        )
        .await;

        unsafe {
            std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST");
            std::env::remove_var("EPICS_PVA_ADDR_LIST");
        }
        ns_handle.abort();

        let resolved = result
            .expect("find() must complete within 5 s; TCP NS search path may be broken")
            .expect("find() must succeed; handle_search_response may not route TCP responses");
        assert_eq!(
            resolved.port(),
            ns_addr.port(),
            "resolved port must match what the NS advertised in SEARCH_RESPONSE"
        );
        assert_eq!(
            resolved.ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            "resolved IP must be 127.0.0.1 (rewrite_loopback from unspecified)"
        );
    }
}
