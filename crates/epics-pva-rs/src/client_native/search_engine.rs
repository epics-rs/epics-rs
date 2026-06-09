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
//! - Per-PV search retry using a 30-bucket ring at 1 s/tick (pvxs
//!   `client.cpp` `searchBuckets`/`tickSearch`). Each retry advances the
//!   channel by `min(attempt, 30)` buckets, giving 1 s, 2 s, 3 s, …,
//!   ~29 s cap (pvxs `nBuckets = 30`). See `cascade_smoothed_next`.
//! - Beacon-driven fast reconnect: when a beacon arrives for a server we
//!   have a disconnected channel against, the engine re-issues SEARCH for
//!   that channel immediately.
//! - Beacon anomaly throttling via [`super::beacon_throttle::BeaconTracker`].

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Interval, interval};
use tracing::{debug, warn};

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    Command, PVA_VERSION, PvaHeader, ReadExt, decode_size, decode_string,
    ip_from_bytes_allow_unspec,
};

use super::beacon_throttle::{BeaconAction, BeaconTracker};
use super::decode::{PeerRole, decode_search_response, try_parse_frame_role};
use super::server_conn::{
    DEFAULT_BUFFER_SIZE, DEFAULT_REGISTRY_SIZE, build_client_connection_validation,
    read_handshake_init, select_client_auth, wait_for_validated,
};

/// Search retry intervals in seconds.
///
/// this constant is NOT used by the engine — actual retry scheduling
/// uses the 30-bucket ring in `run_engine`/`cascade_smoothed_next`
/// (pvxs `client.cpp::tickSearch`), which caps at ~29 s. The values here
/// do not appear in pvxs; the previous doc "matching pvxs clientdiscover.cpp"
/// was incorrect (no such sequence exists there). Retained as public API;
/// do NOT use this to predict channel retry timing.
pub const BACKOFF_SECS: &[u64] = &[1, 1, 2, 5, 10, 15, 30, 60, 120, 210];

/// Default UDP broadcast port for SEARCH/BEACON messages (5076).
pub const DEFAULT_BROADCAST_PORT: u16 = 5076;

/// pvxs `maxSearchPayload` (`client.cpp:43-52`): keep one SEARCH
/// datagram under ~MTU so it is not fragmented. A bucket (or a coalesced
/// initial burst) is packed into as few datagrams as fit under this
/// limit rather than one datagram per channel name.
const MAX_SEARCH_PAYLOAD: usize = 1400;

/// pvxs `initialSearchDelay` (`client.cpp:43`): the first SEARCH for a
/// freshly created channel is deferred by this window so a burst of
/// channel creation coalesces into one batched datagram instead of one
/// datagram per channel. Mirrors `ContextImpl::scheduleInitialSearch`
/// (`client.cpp:766-775`).
const INITIAL_SEARCH_DELAY: Duration = Duration::from_millis(10);

/// Per-frame read deadline for a TCP name-server CONNECTION_VALIDATION
/// handshake when no client timeout is threaded in (the credential-less
/// [`SearchEngine::spawn`] path). Bounds a name server that accepts the TCP
/// connection but never completes the PVA handshake; [`SearchEngine::spawn_with_auth`]
/// supplies the client's own connection timeout instead.
const NS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Resolve `pv_name` → first [`SearchHit`]. Reply via `responder`
    /// once a SEARCH_RESPONSE comes in.
    Find {
        pv_name: String,
        responder: oneshot::Sender<SearchHit>,
        reason: SearchReason,
    },
    /// Resolve `pv_name` and collect *all* responses received within
    /// the next [`MULTI_SERVER_WINDOW`]. The reply contains every
    /// server [`SearchHit`] that claimed the PV; the caller can fan-out /
    /// fail over.
    FindAll {
        pv_name: String,
        responder: oneshot::Sender<Vec<SearchHit>>,
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
    /// Replace the GUID blocklist. Search responses (including discovery
    /// pongs) whose server GUID matches an entry are silently dropped;
    /// BEACONs are NOT filtered. Mirrors pvxs `Context::ignoreServerGUIDs`,
    /// which is consulted only in procSearchReply (client.cpp:880), never
    /// in onBeacon (client.cpp:773-847).
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
    /// least `BEACON_TIMEOUT`, or a known `(server, proto)` reported a new
    /// GUID/peerVersion (the old incarnation timed out). Mirrors pvxs
    /// `Discovered::Timeout` (client.cpp:1272), which carries the full
    /// beacon identity — including `proto` and `peer_version` — so a
    /// consumer can retire the exact `(server, proto)` it tracked. Without
    /// `proto`, a server advertising both `tcp` and `tls` at one endpoint
    /// produces two indistinguishable timeouts and the consumer cannot tell
    /// which protocol went away. `peer_version` is the PVA header version
    /// of the *expired* incarnation (pvxs `cur.peerVersion`).
    Timeout {
        server: SocketAddr,
        guid: [u8; 12],
        proto: String,
        peer_version: u8,
    },
}

/// A resolved `SEARCH_RESPONSE` hit: the full server identity decoded from
/// the reply that claimed the PV, not just its address.
///
/// pvxs decodes `{guid, server, proto, peerVersion}` from every
/// `SEARCH_RESPONSE` and, while the channel is still `Searching`, stores
/// the reply GUID directly on the channel (`chan->guid = guid`,
/// client.cpp:925-927) so later duplicate / server-replacement checks
/// compare against the GUID that *actually resolved the channel*. Returning
/// a bare `SocketAddr` from `find()` forced the channel to re-derive the
/// GUID from the beacon tracker (`guid_for(addr)`), which can be absent
/// (no beacon yet) or — since the tracker is keyed by `(server, proto)` —
/// an arbitrary protocol's GUID. Carrying the hit makes the channel's
/// `expected_guid` come from the search reply, as pvxs intends.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Advertised TCP server endpoint (loopback-rewritten to the UDP peer
    /// when the payload advertised `0.0.0.0`).
    pub server: SocketAddr,
    /// Server GUID decoded from this `SEARCH_RESPONSE`.
    pub guid: [u8; 12],
    /// Advertised transport protocol of the reply (always `"tcp"` for a
    /// found=true reply the engine acts on; see the `proto != "tcp"` gate).
    pub proto: String,
    /// PVA header version of the reply frame (pvxs `peerVersion`).
    pub peer_version: u8,
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
    /// Spawn a search engine without TCP name-server credentials: name-server
    /// handshakes still select `"ca"` if the server offers it but send empty
    /// user/host. The public entry point for discovery-only callers that
    /// configure no name servers (`name_servers` empty → no NS handshake runs).
    pub async fn spawn(
        extra_targets: Vec<SocketAddr>,
        name_servers: Vec<SocketAddr>,
    ) -> PvaResult<Self> {
        Self::spawn_inner(
            extra_targets,
            name_servers,
            String::new(),
            String::new(),
            NS_HANDSHAKE_TIMEOUT,
        )
        .await
    }

    /// Spawn with the client's CA credentials for TCP name-server connections,
    /// so a name-server handshake authenticates as the same user/host as a
    /// normal server connection. pvxs builds name-server peers with the same
    /// `Connection::build()` and auth negotiation as ordinary TCP servers and
    /// only flips `nameserver = true` afterward (client.cpp:674-685); there is
    /// no separate name-server auth policy.
    pub async fn spawn_with_auth(
        extra_targets: Vec<SocketAddr>,
        name_servers: Vec<SocketAddr>,
        ns_user: String,
        ns_host: String,
        ns_handshake_timeout: Duration,
    ) -> PvaResult<Self> {
        Self::spawn_inner(
            extra_targets,
            name_servers,
            ns_user,
            ns_host,
            ns_handshake_timeout,
        )
        .await
    }

    async fn spawn_inner(
        mut extra_targets: Vec<SocketAddr>,
        name_servers: Vec<SocketAddr>,
        ns_user: String,
        ns_host: String,
        ns_handshake_timeout: Duration,
    ) -> PvaResult<Self> {
        let beacons = BeaconTracker::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SearchCommand>(256);

        // `EPICS_PVA_INTF_ADDR_LIST` (v4 entries) constrains the
        // auto-broadcast address expansion (in `search_targets`) and the
        // limited-broadcast / multicast fanout egress (in `broadcast`) to
        // the listed interfaces. It does NOT reduce the active-search
        // socket bundle: pvxs binds the search socket to wildcard
        // (`client.cpp:578-590`) and applies `client::Config::interfaces`
        // only to `expandAddrList` / `addGroups` (`config.cpp:624-648`)
        // and beacon receive, sending every explicit `EPICS_PVA_ADDR_LIST`
        // unicast target through the wildcard socket via the OS route.
        // Constraining the bundle forced an explicit non-loopback target
        // onto a loopback-only socket under `INTF_ADDR_LIST=127.0.0.1`.
        // Regression R0604-PVA-CLIENT-INTF-EXPLICIT-ADDR-1.
        let client_interfaces: Vec<Ipv4Addr> = crate::config::env::list_intf_addresses()
            .into_iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            })
            .collect();

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
        let auto_on = crate::config::env::auto_addr_list_enabled();
        let env_addrs = std::env::var("EPICS_PVA_ADDR_LIST").ok();
        // PVA-466: expand $(VAR) before checking emptiness so an
        // unset macro collapses to "" and the no-destinations
        // warning fires correctly.
        let env_has_dest = env_addrs
            .as_deref()
            .map(|s| crate::config::env::expand_dollar_vars(s))
            // Whitespace-only token test (pvxs `split_addr_into`): the comma
            // and `@` are endpoint modifiers, so a token contributes a
            // destination only when its address part (before any `,`/`@`) is
            // non-empty — matching `Endpoint::parse`, which drops a token
            // with an empty address. A bare-comma string is still "no dest".
            .map(|s| {
                s.split_whitespace()
                    .any(|t| !t.split([',', '@']).next().unwrap_or("").is_empty())
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
            ns_user,
            ns_host,
            ns_handshake_timeout,
            client_interfaces,
        ));

        Ok(Self { cmd_tx, beacons })
    }

    /// Issue a search for `pv_name`. Future resolves to the resolving
    /// [`SearchHit`] (server identity decoded from the SEARCH_RESPONSE)
    /// once a response arrives. `reason` controls whether the first
    /// SEARCH packet fires immediately (`Initial`) or is bucket-spread
    /// (`Reconnect`).
    pub async fn find(&self, pv_name: &str, reason: SearchReason) -> PvaResult<SearchHit> {
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
    pub async fn find_all(&self, pv_name: &str, reason: SearchReason) -> PvaResult<Vec<SearchHit>> {
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
    /// replacement at the same address. None when the
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

    /// Set the server-GUID blocklist. SEARCH_RESPONSE frames (including
    /// discovery pongs) from a server whose GUID is on this list are
    /// silently dropped; BEACONs from that server still flow into the
    /// tracker and `discover()` stream. Mirrors pvxs
    /// `Context::ignoreServerGUIDs` — "Ignore any search replies with
    /// these GUIDs" (client.h:593-595), consulted only in procSearchReply.
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
    /// The receiver is bounded; if the consumer falls behind, individual
    /// events are dropped silently, but the **subscription survives** — a
    /// momentarily-full queue does not unsubscribe a live consumer (pvxs
    /// keeps each discovery operation until the caller cancels it,
    /// clientdiscover.cpp:103-112). The subscription ends only when the
    /// receiver is dropped.
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
    //
    // The bundle is ALWAYS all-NIC, matching pvxs which binds the
    // active-search socket to wildcard (`client.cpp:578-590`).
    // `EPICS_PVA_INTF_ADDR_LIST` does NOT reduce this bundle: pvxs
    // applies the interface list only to auto-broadcast expansion
    // (`config.cpp:624-648`) and beacon receive, and sends every explicit
    // `EPICS_PVA_ADDR_LIST` unicast target through the wildcard socket via
    // the OS route. Reducing the bundle to the interface list forced an
    // explicit non-loopback target onto a loopback-only socket under
    // `INTF_ADDR_LIST=127.0.0.1`; the interface constraint now lives only
    // in `search_targets` (auto-broadcast generation) and `broadcast`'s
    // limited-broadcast / multicast fanout egress. Regression
    // R0604-PVA-CLIENT-INTF-EXPLICIT-ADDR-1.
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
    let port = crate::config::env::broadcast_port();
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

    let port = crate::config::env::broadcast_port();

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

/// Project `EPICS_PVA_ADDR_LIST` endpoints to their multicast join
/// targets. Each entry is `(group, iface)`:
///
/// * `iface = Some(ip)` — the endpoint carried an explicit `@iface`
///   modifier, so the group is joined on *that interface alone* (pvxs
///   joins the one chosen interface; `udp_collector.cpp:186-196`).
/// * `iface = None` — a modifier-less group, joined on every external NIC.
///
/// Non-multicast and IPv6 entries are dropped (the v4 socket cannot join a
/// v6 group, and a unicast/broadcast search target needs no membership).
/// An unresolvable `@iface` spec is skipped (logged). Pure given IP-literal
/// `@iface` forms (`resolve_iface_v4` passthrough), so the projection is
/// testable without real NIC multicast.
fn addr_list_multicast_targets(
    endpoints: &[crate::config::Endpoint],
) -> Vec<(Ipv4Addr, Option<Ipv4Addr>)> {
    let mut out = Vec::new();
    for ep in endpoints {
        let SocketAddr::V4(v4) = ep.addr else {
            continue;
        };
        let group = *v4.ip();
        if !group.is_multicast() {
            continue;
        }
        let iface = match ep.iface.as_deref() {
            None => None,
            Some(spec) => match crate::config::env::resolve_iface_v4(spec) {
                Ok(ip) => Some(ip),
                Err(e) => {
                    debug!(
                        "EPICS_PVA_ADDR_LIST {group}@{spec}: \
                         interface resolve failed: {e}; skipping join"
                    );
                    continue;
                }
            },
        };
        out.push((group, iface));
    }
    out
}

/// Walk `EPICS_PVA_ADDR_LIST` and join its IPv4 multicast groups on `sock`.
/// Errors are logged but not propagated — a single failed join shouldn't
/// disable the rest of the discovery path.
///
/// A group with an explicit `@iface` modifier is joined on that interface
/// alone via [`AsyncUdpV4::join_multicast_v4_on`]; a modifier-less group is
/// joined on every external NIC. Regression
/// R0604-PVASRV-MCAST-JOIN-IFACE-1: the previous code dropped the `@iface`
/// and called the all-NIC [`AsyncUdpV4::join_multicast_v4`] for every
/// group — the same `@iface`-dropping defect as the server beacon-join
/// path, fixed here in the same change.
pub(crate) fn join_addr_list_multicast(sock: &AsyncUdpV4) {
    let Ok(env) = std::env::var("EPICS_PVA_ADDR_LIST") else {
        return;
    };
    // Route through the single address-list parser
    // (`parse_endpoints_with_port`) instead of a private comma-split +
    // `:`-split: it splits on WHITESPACE only (pvxs `split_addr_into` —
    // the comma is endpoint syntax, not a list separator), expands
    // `$(VAR)` macros, and resolves DNS / bracketed-v6 the same way the
    // active-SEARCH path does. We keep only the V4 multicast groups.
    let bport = crate::config::env::broadcast_port();
    let endpoints = crate::config::env::parse_endpoints_with_port(&env, bport);
    for (group, iface) in addr_list_multicast_targets(&endpoints) {
        match iface {
            Some(iface_ip) => match sock.join_multicast_v4_on(group, iface_ip) {
                Ok(()) => debug!("joined multicast group {group} on interface {iface_ip}"),
                Err(e) => debug!("join_multicast_v4_on {group}@{iface_ip} failed: {e}"),
            },
            None => {
                if let Err(e) = sock.join_multicast_v4(group) {
                    debug!("join_multicast_v4 for {group} failed: {e}");
                } else {
                    debug!("joined multicast group {group}");
                }
            }
        }
    }
}

// ── Engine main loop ────────────────────────────────────────────────────

enum Responder {
    Single(oneshot::Sender<SearchHit>),
    Multi {
        responder: oneshot::Sender<Vec<SearchHit>>,
        accumulated: Vec<SearchHit>,
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

/// pvxs `pokeHoldoff` (client.cpp:60): global minimum interval between two
/// fast-search pokes. A beacon-driven poke is ignored unless at least this
/// long has elapsed since the last granted poke, so a site-wide IOC reboot
/// (many fresh `(server, guid)` beacons at once) cannot drive a sustained
/// amplified UDP search-broadcast storm.
const POKE_HOLDOFF: Duration = Duration::from_secs(30);

/// pvxs fixed SEARCH sequence id `search_seq` (client.cpp:71-73): the
/// ASCII bytes `"find"` (0x66 0x69 0x6e 0x64). pvxs stamps this single
/// value into EVERY outgoing SEARCH — regular and discovery alike — via
/// the shared `tickSearch` emit loop (client.cpp:1072), with the comment
/// "searchSequenceID in CMD_SEARCH is redundant. So we use a static
/// value". It also only treats a `found=false`, zero-channel UDP
/// SEARCH_RESPONSE as a discovery pong when its decoded sequence equals
/// `search_seq` (client.cpp:889). Using a fixed, recognizable value lets
/// the pong path reject stray/unrelated `found=false` replies instead of
/// promoting any of them to a fake beacon.
const SEARCH_SEQ: u32 = 0x6669_6e64;

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

/// Single owner of a search's post-send retry decision: returns the
/// `(next_bucket, next_attempt)` to requeue it with.
///
/// pvxs `tickSearch(..., poked)` (client.cpp:1141-1160): on a poked tick (during
/// the fast poke revolution) `ninc = 0`, so the channel keeps its accumulated
/// `nSearch` backoff and is requeued into the SAME bucket — because
/// `current_bucket` has already advanced, it waits for the fast revolution to
/// wrap rather than becoming a fresh 1-bucket retry. On a normal tick `nSearch`
/// is incremented first and the channel escalates forward by that count with
/// cascade smoothing (client.cpp:1193-1206).
fn rearm_after_send(
    poked: bool,
    current_bucket: usize,
    attempt: u32,
    bucket_sizes: impl Fn(usize) -> usize,
) -> (usize, u32) {
    if poked {
        (current_bucket, attempt)
    } else {
        let next_attempt = attempt.saturating_add(1);
        (
            cascade_smoothed_next(current_bucket, next_attempt, bucket_sizes),
            next_attempt,
        )
    }
}

/// Deduplicate name-server targets, preserving first-seen order.
///
/// pvxs parses `EPICS_PVA_NAME_SERVERS` through `split_addr_into`
/// (config.cpp:148-183), which resolves hostnames, re-prints each as a
/// canonical address, then `std::sort`s and `std::unique`s the list — so a
/// duplicate token (`"host:5075 host:5075"`) collapses to ONE entry and
/// `startNS` (client.cpp:651-667) opens exactly one persistent TCP
/// connection per unique target. Our `name_servers()` returned the parsed
/// vector unchanged, so a duplicate token spawned two `ns_task`s and
/// double-forwarded every SEARCH. Dedup at the spawn gate — the single
/// owner of the list→connections transition — restores one-connection-per-
/// unique-target for both env-derived and programmatic
/// `PvaClientBuilder::name_servers()` lists. Order is preserved rather than
/// sorted: name servers are queried in parallel, so pvxs's canonical sort is
/// not observable, and first-seen order keeps the config's intent.
///
/// Regression R0604-PVA-NAMESERVER-DEDUP-1.
fn dedup_name_servers(name_servers: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = std::collections::HashSet::with_capacity(name_servers.len());
    name_servers
        .into_iter()
        .filter(|a| seen.insert(*a))
        .collect()
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
    ns_user: String,
    ns_host: String,
    ns_handshake_timeout: Duration,
    // `EPICS_PVA_INTF_ADDR_LIST` (v4 entries): when non-empty, the auto
    // broadcast-target expansion is restricted to these interfaces'
    // subnets. Empty = all-NIC default.
    client_interfaces: Vec<Ipv4Addr>,
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
    // The v6 SEARCH socket (`bind_ephemeral_udp_v6` → `[::]:0`) binds its
    // own ephemeral port, distinct from the v4 search socket's shared
    // port. A SEARCH frame must advertise the response port of the socket
    // it is transmitted on (pvxs honours the advertised `response_port`
    // via `udp_collector.cpp:380 setPort`), so v6 frames carry the v6
    // socket's port while v4 frames keep `response_port`. With v6 disabled
    // this equals `response_port`, so v6/v4 frames are byte-identical.
    // Regression R0604-PVASRV-V6-WILDCARD-SEARCH-PORT-1.
    let response_port_v6 = search_socket_v6
        .as_ref()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(response_port);

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
    // Initial-search coalescing window (pvxs `initialSearchBucket` +
    // `scheduleInitialSearch`, client.cpp:766-775). Freshly created
    // channels accrue here for INITIAL_SEARCH_DELAY, then their first
    // SEARCH goes out as one batched datagram instead of one per PV.
    // `initial_deadline` is the instant the window flushes; `None` means
    // no window is armed. The sids are also placed in their retry
    // buckets at command time, so this affects only the first broadcast.
    let mut initial_pending: Vec<u32> = Vec::new();
    let mut initial_deadline: Option<Instant> = None;
    // Server-GUID blocklist (pvxs `ignoreServerGUIDs`). Only SEARCH_RESPONSE
    // frames (incl. discovery pongs) with a matching GUID are dropped;
    // BEACONs are not filtered (pvxs onBeacon has no ignore check).
    // HashSet lookup keeps the steady-state cost negligible.
    let mut ignore_guids: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
    // After a `poke()` (fresh server identity discovered) we run one
    // 30-bucket revolution at fast 200 ms cadence so all pending
    // searches retry within 6 s instead of up to 30 s. Counter
    // decrements per fast tick; reaches 0 → revert to 1 s cadence.
    let mut fast_ticks_remaining: u32 = 0;
    // Wall-clock of the last GRANTED poke (pvxs `lastPoke`, client.cpp:750).
    // `None` = never poked, so the first poke is always allowed. Gates the
    // `POKE_HOLDOFF` rate limit inside `maybe_poke`.
    let mut last_poke: Option<Instant> = None;
    // Beacon-identity de-duplication is owned by `beacons` (BeaconTracker):
    // `observe()` returns New/Changed/Update so there is no separate
    // "already announced" set to keep in sync (pvxs drives discover()
    // emission off the same beaconTrack New/Change/Update classification,
    // client.cpp:784-847).

    // pvxs client.cpp:651-667 startNS(): one persistent TCP connection per
    // EPICS_PVA_NAME_SERVERS entry. Each ns_task handles connect/reconnect;
    // ns_senders receives SEARCH frame bytes to forward over the connection.
    // Collapse duplicate name-server targets to one TCP connection each, matching
    // pvxs `split_addr_into` sort+unique (config.cpp:179-183). See
    // `dedup_name_servers`. Regression R0604-PVA-NAMESERVER-DEDUP-1.
    let name_servers = dedup_name_servers(name_servers);
    let (ns_response_tx, mut ns_response_rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(64);
    let mut ns_senders: Vec<NsHandle> = Vec::with_capacity(name_servers.len());
    for ns_addr in name_servers {
        let (search_tx, search_rx) = mpsc::channel::<Vec<u8>>(64);
        // Per-NS readiness gate. pvxs (client.cpp:1221-1225) skips a name
        // server during a search tick unless `serv->ready && serv->connection()`
        // are both true, and keeps no disconnected-side queue (client.cpp:1227-1235).
        // The flag is owned by `ns_task`: false until CONNECTION_VALIDATED, false
        // again the instant the connection drops. Without it, the bounded channel
        // buffered up to 64 stale SEARCH frames while the NS was offline or still
        // handshaking and replayed them as a burst on reconnect.
        let ready = Arc::new(AtomicBool::new(false));
        ns_senders.push(NsHandle {
            tx: search_tx,
            ready: Arc::clone(&ready),
        });
        let resp_tx = ns_response_tx.clone();
        tokio::spawn(ns_task(
            ns_addr,
            search_rx,
            resp_tx,
            ready,
            ns_user.clone(),
            ns_host.clone(),
            ns_handshake_timeout,
        ));
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

        // Fire when the coalescing window for buffered initial searches
        // elapses (pvxs `initialSearcher` timer). `None` ⟹ no window
        // armed, so this arm parks forever.
        let initial_arm = async {
            match initial_deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(SearchCommand::Find { pv_name, responder, reason }) => {
                    // drop any prior pending search for the
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
                        }
                        // Defer the first broadcast into the coalescing
                        // window instead of sending one datagram now — a
                        // burst of channel creation is packed into one
                        // batched SEARCH on flush (pvxs initialSearcher).
                        initial_pending.push(sid);
                        if initial_deadline.is_none() {
                            initial_deadline = Some(Instant::now() + INITIAL_SEARCH_DELAY);
                        }
                    }
                }
                Some(SearchCommand::FindAll { pv_name, responder, reason }) => {
                    // same dedup as Find — drop any prior
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
                        }
                        // Coalesce with any other initial searches in the
                        // window (see the Find branch above).
                        initial_pending.push(sid);
                        if initial_deadline.is_none() {
                            initial_deadline = Some(Instant::now() + INITIAL_SEARCH_DELAY);
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
                    // pvxs `ignoreServerGUIDs` is a SEARCH_RESPONSE-only
                    // filter (client.cpp:880, in procSearchReply): a beacon
                    // from an ignored GUID still runs through `onBeacon()`,
                    // enters the tracker, fires `Discovered::Online`, and
                    // pokes pending searches (client.cpp:773-847 has no
                    // ignore check). So this in-process beacon injection
                    // does NOT consult the blocklist.
                    // BeaconObserved is the in-process injection path (e.g.
                    // a co-located server) — there's no UDP datagram, so
                    // peer == server, proto defaults to "tcp", and the peer
                    // version is our own protocol version. The tracker
                    // classifies New/Changed/Update and `emit_beacon_action`
                    // turns that into the right Discovered events.
                    let action = beacons.observe(server, "tcp", guid, PVA_VERSION);
                    let should_poke = emit_beacon_action(
                        action,
                        server,
                        guid,
                        server,
                        "tcp".into(),
                        &mut subscribers,
                    );
                    if should_poke {
                        // pvxs `poke()` (client.cpp:736-759): a fresh/changed
                        // server identity starts the 200 ms fast search
                        // revolution and records the poke time — it does NOT
                        // touch per-channel `nSearch`. The fast cadence sweeps
                        // the ring so every parked search retransmits within
                        // one revolution while keeping its accumulated backoff.
                        // `maybe_poke` enforces the 30 s holdoff + one-active
                        // -revolution guard.
                        maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
                    }
                }
                Some(SearchCommand::Subscribe { responder }) => {
                    let (tx, rx) = mpsc::channel::<Discovered>(64);
                    subscribers.push(tx);
                    let _ = responder.send(rx);
                }
                Some(SearchCommand::HurryUp) => {
                    // pvxs `hurryUp()` routes through the same rate-limited
                    // `poke()` (client.cpp:736-759): it is equally subject to the
                    // 30 s holdoff and the skip-while-active guard. Start the fast
                    // revolution WITHOUT resetting per-PV backoff: the fast cadence
                    // retransmits every pending search within ~6 s while preserving
                    // its `nSearch` state (the tick handler's poked branch skips
                    // the increment and requeues into the same bucket). The prior
                    // `attempt = 0` reset was more aggressive than pvxs's poked
                    // semantic.
                    maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
                }
                Some(SearchCommand::CacheClear { pv_name }) => {
                    // Same drop-the-name path as Cancel, but the name
                    // is the public identifier. An empty name is a
                    // wildcard over every pending search, matching pvxs
                    // cacheClean's `name.empty()` skip-the-filter rule
                    // (client.cpp:1341-1348).
                    if pv_name.is_empty() {
                        by_name.clear();
                        pending.clear();
                        for bucket in &mut search_buckets {
                            bucket.clear();
                        }
                    } else if let Some(sid) = by_name.remove(&pv_name) {
                        if let Some(p) = pending.remove(&sid) {
                            search_buckets[p.bucket].retain(|x| *x != sid);
                        }
                    }
                }
                Some(SearchCommand::IgnoreServerGuids { guids }) => {
                    // Replace (not merge) so callers can also CLEAR
                    // the list with an empty Vec. pvxs `ignoreServerGUIDs`
                    // (client.cpp:454-460) only stores the vector; it has
                    // no side effect on beacon tracking, because beacons
                    // from ignored GUIDs are still reported through
                    // discover(). So we do NOT touch the BeaconTracker here.
                    ignore_guids = guids.into_iter().collect();
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
                    //
                    // Stamp the fixed pvxs `search_seq` ("find") rather than
                    // a fresh randomized id: the discovery-pong receive path
                    // gates on this exact value (client.cpp:889) so an
                    // unrelated found=false reply is not promoted to a fake
                    // beacon.
                    let pkt_v4 = codec.build_discover_search(SEARCH_SEQ, response_port);
                    let pkt_v6 = codec.build_discover_search(SEARCH_SEQ, response_port_v6);
                    broadcast(&search_socket, search_socket_v6.as_ref(), &pkt_v4, &pkt_v6, &extra_targets, &client_interfaces, &mut search_send_errs).await;
                }
                None => break,
            },

            res = search_socket.recv_from(&mut search_buf) => {
                if let Ok((n, peer)) = res {
                    // Multi-message drain: pvxs packs many
                    // SEARCH messages per UDP datagram. Without the
                    // loop we'd parse only the first and silently
                    // drop the rest.
                    let mut poke = false;
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_search_response(
                            &search_buf[pos..n],
                            &mut pending, &mut by_name, &beacons, &ignore_guids,
                            &mut subscribers, &mut poke, peer, false,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke {
                        // pvxs `poke()` (client.cpp:736-759): rate-limited.
                        // Never extends a revolution already in flight.
                        maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
                    }
                }
            }

            res = recv_from_v6_opt(search_socket_v6.as_ref(), &mut search_buf_v6) => {
                // PR #205 IPv6 Stage 4: v6 SEARCH_RESPONSE arrives
                // unicast back to this v6 socket. Decode reuses the
                // same family-agnostic handler.
                if let Some(Ok((n, peer))) = res {
                    let mut poke = false;
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_search_response(
                            &search_buf_v6[pos..n],
                            &mut pending, &mut by_name, &beacons, &ignore_guids,
                            &mut subscribers, &mut poke, peer, false,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke {
                        // pvxs `poke()` (client.cpp:736-759): rate-limited.
                        // Never extends a revolution already in flight.
                        maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
                    }
                }
            }

            res = beacon_recv => {
                if let Ok((n, from)) = res {
                    let mut poke = false;
                    // Multi-message drain: same rationale as
                    // search responses — beacons can be chained.
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_beacon(
                            &beacon_buf[pos..n], &beacons,
                            &mut subscribers, &mut poke,
                            from,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke {
                        // pvxs `poke()` (client.cpp:736-759): rate-limited.
                        // Never extends a revolution already in flight.
                        maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
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
                            &beacon_buf_v6[pos..n], &beacons,
                            &mut subscribers, &mut poke,
                            from,
                        );
                        if consumed == 0 {
                            break;
                        }
                        pos = pos.saturating_add(consumed);
                    }
                    if poke {
                        // pvxs `poke()` (client.cpp:736-759): rate-limited.
                        // Never extends a revolution already in flight.
                        maybe_poke(&mut tick, &mut fast_ticks_remaining, &mut last_poke).await;
                    }
                }
            }

            _ = beacon_clean_tick.tick() => {
                for (server, proto, guid, peer_version) in beacons.prune_stale(BEACON_TIMEOUT) {
                    let evt = Discovered::Timeout { server, guid, proto, peer_version };
                    publish_discovery(&mut subscribers, evt);
                }
            }

            ns_rsp = ns_response_rx.recv() => {
                // SEARCH_RESPONSE received over a TCP name-server connection.
                // pvxs client.cpp:984-995: procSearchReply with istcp=true.
                // is_tcp=true so the discovery-pong path does not fire here.
                if let Some((bytes, ns_addr)) = ns_rsp {
                    let mut poke = false;
                    handle_search_response(
                        &bytes, &mut pending, &mut by_name, &beacons, &ignore_guids,
                        &mut subscribers, &mut poke, ns_addr, true,
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

            _ = initial_arm => {
                // The initial-search coalescing window elapsed: send the
                // burst as one batched SEARCH (broadcast + name-server
                // forward), then disarm until the next initial Find.
                flush_initial_searches(
                    &mut initial_pending,
                    &pending,
                    &codec,
                    response_port,
                    response_port_v6,
                    &search_socket,
                    search_socket_v6.as_ref(),
                    &extra_targets,
                    &client_interfaces,
                    &mut search_send_errs,
                    &ns_senders,
                )
                .await;
                initial_deadline = None;
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
                // pvxs `tickSearch(..., poked)` (client.cpp:1141-1160): a tick
                // during the fast poke revolution skips the `nSearch++` backoff
                // increment and requeues each channel into the SAME bucket. Since
                // `current_bucket` has already advanced, the channel waits for the
                // fast revolution to wrap rather than becoming a fresh 1-bucket
                // retry. Preserving `attempt` keeps each search's accumulated
                // backoff across the poke.
                let poked = fast_ticks_remaining > 0;
                let bucket_ids = std::mem::take(&mut search_buckets[current_bucket]);
                // Phase 1: prune dead responders, re-anchor first-attempt
                // FindAll deadlines, and collect the live (sid, name)
                // entries so the whole bucket goes out as ONE batched
                // SEARCH rather than one datagram per channel per
                // destination.
                let mut to_send: Vec<(u32, String)> = Vec::with_capacity(bucket_ids.len());
                let mut rearm_ids: Vec<u32> = Vec::with_capacity(bucket_ids.len());
                for sid in bucket_ids {
                    let responder_dead = match pending.get(&sid) {
                        // drop searches whose oneshot responder
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

                    if let Some(p) = pending.get_mut(&sid) {
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
                        to_send.push((sid, p.pv_name.clone()));
                        rearm_ids.push(sid);
                    }
                }

                // Phase 2: pack the bucket into ≤MAX_SEARCH_PAYLOAD
                // datagrams (channel count once per datagram) and
                // broadcast each; reuse the packed names for name-server
                // forwarding with the response port zeroed.
                if !to_send.is_empty() {
                    // Pack the same entries with each family's response
                    // port. The port field is fixed-width, so the two sets
                    // batch identically and pair 1:1 (see `broadcast`).
                    let frames_v4 = pack_search_frames(&codec, &to_send, response_port, false);
                    let frames_v6 = pack_search_frames(&codec, &to_send, response_port_v6, false);
                    for (frame_v4, frame_v6) in frames_v4.iter().zip(&frames_v6) {
                        broadcast(
                            &search_socket,
                            search_socket_v6.as_ref(),
                            frame_v4,
                            frame_v6,
                            &extra_targets,
                            &client_interfaces,
                            &mut search_send_errs,
                        )
                        .await;
                    }
                    ns_forward_frames(&ns_senders, &pack_search_frames(&codec, &to_send, 0, true));
                }

                // Phase 3: re-arm each sent search into its next retry
                // bucket via the single owner of the post-send retry
                // decision. Done in send order so the cascade-smoothing
                // rule observes bucket sizes grow exactly as before
                // (broadcast does not mutate the buckets, so deferring
                // the re-arm past the sends is size-equivalent).
                for sid in rearm_ids {
                    if let Some(p) = pending.get(&sid) {
                        let attempt = p.attempt;
                        let (next, next_attempt) = {
                            let bucket_sizes = |idx: usize| search_buckets[idx].len();
                            rearm_after_send(poked, current_bucket, attempt, bucket_sizes)
                        };
                        // Update the pending's bucket/attempt BEFORE the
                        // push so the in-place state and the buckets agree.
                        if let Some(p) = pending.get_mut(&sid) {
                            p.attempt = next_attempt;
                            p.bucket = next;
                        }
                        search_buckets[next].push(sid);
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

/// pvxs `ContextImpl::poke()` (client.cpp:736-759): the single, rate-limited
/// gate for the fast search revolution. Every poke trigger (fresh-server
/// beacon, `HurryUp`, or a `poke`-flagged SEARCH_RESPONSE / beacon datagram)
/// routes through here, so the two pvxs guards hold by construction:
///
/// - **skip-while-active** (`if(nPoked) return`): if a fast revolution is
///   still running (`*fast_ticks_remaining > 0`) the poke is declined — the
///   current revolution is never re-armed or extended mid-flight.
/// - **30 s holdoff** (`age < pokeHoldoff`): the poke is declined unless at
///   least [`POKE_HOLDOFF`] has elapsed since the last GRANTED poke.
///
/// Returns `true` iff the poke was granted, in which case it has already
/// recorded the poke time, switched `tick` to the 200 ms fast cadence, and
/// armed one full revolution; the caller may then bring pending searches
/// forward. Returns `false` (no side effects) when either guard declines —
/// this is what stops a site-wide IOC reboot from amplifying into a sustained
/// UDP search-broadcast storm.
async fn maybe_poke(
    tick: &mut Interval,
    fast_ticks_remaining: &mut u32,
    last_poke: &mut Option<Instant>,
) -> bool {
    if *fast_ticks_remaining != 0 {
        // A revolution is in flight — let it finish (pvxs `if(nPoked) return`).
        return false;
    }
    let now = Instant::now();
    if let Some(prev) = *last_poke {
        if now.duration_since(prev) < POKE_HOLDOFF {
            // Inside the global holdoff window — ignore (pvxs `age < pokeHoldoff`).
            return false;
        }
    }
    *last_poke = Some(now);
    *tick = interval(Duration::from_millis(200));
    tick.tick().await; // skip the immediate fire so cadence doesn't double-tick
    *fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
    true
}

/// Build the deduplicated UDP SEARCH destination list for one broadcast
/// tick.
///
/// pvxs sends SEARCH only to the effective address list: the configured
/// `addressList` (here `extra_targets`, parsed and DNS-resolved once at
/// `SearchEngine::spawn`) plus auto-added broadcast destinations ONLY when
/// `EPICS_PVA_AUTO_ADDR_LIST` is enabled. pvxs skips `expandAddrList()`
/// entirely when `autoAddrList` is false (config.cpp:624-643) and sends
/// only to `effective.addressList` (client.cpp:601-619) — there is NO
/// unconditional `255.255.255.255` fallback. So with `AUTO_ADDR_LIST=NO`
/// and an empty address list this returns an EMPTY list and no SEARCH is
/// emitted, instead of leaking limited-broadcast traffic onto a LAN the
/// operator intentionally restricted.
fn search_targets(
    bport: u16,
    auto_addr_list: bool,
    extra_targets: &[SocketAddr],
    client_interfaces: &[Ipv4Addr],
) -> Vec<SocketAddr> {
    let mut targets: Vec<SocketAddr> = Vec::with_capacity(8);

    // Auto-expansion destinations — gated on AUTO_ADDR_LIST, matching
    // pvxs's `expandAddrList()` which only runs when autoAddrList is true.
    if auto_addr_list && client_interfaces.is_empty() {
        // All-NIC default (`EPICS_PVA_INTF_ADDR_LIST` unset).
        // Limited broadcast. pvxs uses per-interface directed broadcasts;
        // the 255.255.255.255 + per-NIC fanout below is the Rust
        // cross-NIC equivalent. On multi-NIC hosts (and macOS) the kernel
        // may not translate 255.255.255.255 to every NIC's per-subnet
        // broadcast, so we also enumerate each up-non-loopback NIC's IPv4
        // broadcast address (otherwise local IOCs on `192.168.X.255:5076`
        // are never reached).
        targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), bport));
        for sa in crate::config::env::list_broadcast_addresses(bport) {
            // The helper appends 255.255.255.255 too — the dedup below
            // collapses the duplicate.
            targets.push(sa);
        }
        // No implicit loopback unicast here. pvxs's auto-address expansion
        // (config.cpp:624-648 → expandAddrList → evhelper.cpp:625-660)
        // returns only discovered broadcast addresses, never a hard-coded
        // 127.0.0.1 target. Loopback-only discovery requires an explicit
        // operator entry: EPICS_PVA_ADDR_LIST=127.0.0.1 (flows into
        // `extra_targets` below) or loopback in EPICS_PVA_INTF_ADDR_LIST
        // (the gated push in the branch below). Injecting loopback by
        // default let a Rust client reach loopback-only IOCs that pvxs
        // would not, breaking isolation on shared hosts.
    } else if auto_addr_list {
        // `EPICS_PVA_INTF_ADDR_LIST` set: pvxs expands the auto address
        // list over the configured interfaces only (`config.cpp:624-648`),
        // so the directed-broadcast set is restricted to the listed
        // interfaces' subnets. `list_broadcast_addresses_on` adds the
        // limited-broadcast 255.255.255.255 fallback iff a non-loopback
        // interface is listed — a loopback-only list contributes none, so
        // no broadcast leaves the host.
        for sa in crate::config::env::list_broadcast_addresses_on(client_interfaces, bport) {
            targets.push(sa);
        }
        // Loopback unicast only when loopback is an explicitly-listed
        // interface (the all-NIC path adds it unconditionally as a
        // zero-config convenience; here the operator chose the set).
        if client_interfaces.iter().any(|ip| ip.is_loopback()) {
            targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bport));
        }
    }

    // Explicitly configured / programmatic targets are always sent — they
    // are the `addressList` itself, present regardless of autoAddrList.
    for &t in extra_targets {
        targets.push(t);
    }

    // Dedup while preserving insertion order.
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert(*t));
    targets
}

/// Fan a limited-broadcast / multicast SEARCH out of exactly the
/// `EPICS_PVA_INTF_ADDR_LIST` interfaces. Loopback is skipped — it cannot
/// carry broadcast, and `list_broadcast_addresses_on` yields no broadcast
/// target for a loopback-only list, so this is never reached for one.
/// Best-effort: succeeds if any listed interface accepted the send, else
/// returns the last error. With the all-NIC search bundle this reproduces
/// the egress of the pre-fix interface-constrained bundle's `fanout_to`,
/// while leaving explicit unicast to route across the full bundle via
/// `pick_nic` (Regression R0604-PVA-CLIENT-INTF-EXPLICIT-ADDR-1).
async fn fanout_on_interfaces(
    socket: &AsyncUdpV4,
    packet: &[u8],
    dest: SocketAddr,
    client_interfaces: &[Ipv4Addr],
) -> std::io::Result<()> {
    let mut ok = 0usize;
    let mut last_err: Option<std::io::Error> = None;
    for ip in client_interfaces {
        if ip.is_loopback() {
            continue;
        }
        match socket.send_via(packet, dest, *ip).await {
            Ok(_) => ok += 1,
            Err(e) => last_err = Some(e),
        }
    }
    if ok > 0 {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "no listed EPICS_PVA_INTF_ADDR_LIST interface available for broadcast fanout",
            )
        }))
    }
}

async fn broadcast(
    socket: &AsyncUdpV4,
    socket_v6: Option<&Arc<UdpSocket>>,
    // The SEARCH frame to transmit, in two per-family variants: `packet_v4`
    // advertises the v4 search socket's response port and goes to every v4
    // destination; `packet_v6` advertises the v6 socket's port and goes to
    // every v6 destination. The two differ only in the 2-byte
    // `response_port` field (identical length → identical MTU batching),
    // so a caller that packs both with the same entries gets 1:1 frames.
    // Regression R0604-PVASRV-V6-WILDCARD-SEARCH-PORT-1.
    packet_v4: &[u8],
    packet_v6: &[u8],
    extra_targets: &[SocketAddr],
    client_interfaces: &[Ipv4Addr],
    send_errs: &mut HashSet<SocketAddr>,
) {
    let bport = crate::config::env::broadcast_port();
    let targets = search_targets(
        bport,
        crate::config::env::auto_addr_list_enabled(),
        extra_targets,
        client_interfaces,
    );

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
                    // Limited-broadcast (255.255.255.255) / multicast egress
                    // is constrained to EPICS_PVA_INTF_ADDR_LIST when set, so
                    // the all-NIC search bundle does not leak auto-broadcast
                    // out an interface the operator excluded. Empty list =
                    // every up-non-loopback NIC (the all-NIC default). This
                    // reproduces the pre-fix interface-constrained bundle's
                    // broadcast egress exactly while the bundle itself stays
                    // all-NIC so explicit unicast routes via `pick_nic` below.
                    // Per-subnet directed broadcasts and explicit unicast take
                    // the `send_to` path. Regression
                    // R0604-PVA-CLIENT-INTF-EXPLICIT-ADDR-1.
                    if client_interfaces.is_empty() {
                        socket.fanout_to(packet_v4, t).await.map(|_| ())
                    } else {
                        fanout_on_interfaces(socket, packet_v4, t, client_interfaces).await
                    }
                } else {
                    socket.send_to(packet_v4, t).await.map(|_| ())
                }
            }
            SocketAddr::V6(_) => match socket_v6 {
                Some(s6) => s6.send_to(packet_v6, t).await.map(|_| ()),
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

/// Send the coalesced initial-search burst as one batched SEARCH.
///
/// pvxs `tickSearch(SearchKind::initial)` (`client.cpp:1063-1170`): the
/// channels that accrued during the [`INITIAL_SEARCH_DELAY`] window are
/// packed into as few datagrams as fit under [`MAX_SEARCH_PAYLOAD`] and
/// broadcast once, then forwarded to the name servers (port zeroed).
/// Each sid is already placed in its retry bucket at command time, so
/// retry scheduling and Cancel are unaffected — this only sends the
/// *first* broadcast. A sid that was deduped/cancelled, or whose caller
/// dropped the responder during the window, is filtered out here (its
/// `pending` entry is gone or its responder closed), so no SEARCH leaks
/// for a channel that no longer exists.
#[allow(clippy::too_many_arguments)]
async fn flush_initial_searches(
    initial_pending: &mut Vec<u32>,
    pending: &HashMap<u32, Pending>,
    codec: &PvaCodec,
    response_port: u16,
    response_port_v6: u16,
    search_socket: &AsyncUdpV4,
    search_socket_v6: Option<&Arc<UdpSocket>>,
    extra_targets: &[SocketAddr],
    client_interfaces: &[Ipv4Addr],
    send_errs: &mut HashSet<SocketAddr>,
    ns_senders: &[NsHandle],
) {
    let entries: Vec<(u32, String)> = initial_pending
        .drain(..)
        .filter_map(|sid| {
            pending.get(&sid).and_then(|p| {
                let alive = match &p.responder {
                    Responder::Single(tx) => !tx.is_closed(),
                    Responder::Multi { responder, .. } => !responder.is_closed(),
                };
                alive.then(|| (sid, p.pv_name.clone()))
            })
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    // Pack the same entries with each family's response port; the fixed-
    // width port field keeps the two sets batched identically (1:1).
    let frames_v4 = pack_search_frames(codec, &entries, response_port, false);
    let frames_v6 = pack_search_frames(codec, &entries, response_port_v6, false);
    for (frame_v4, frame_v6) in frames_v4.iter().zip(&frames_v6) {
        broadcast(
            search_socket,
            search_socket_v6,
            frame_v4,
            frame_v6,
            extra_targets,
            client_interfaces,
            send_errs,
        )
        .await;
    }
    ns_forward_frames(ns_senders, &pack_search_frames(codec, &entries, 0, true));
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

/// Deliver a discovery event to every live subscriber.
///
/// Single owner of the subscriber-eviction policy. pvxs runs each
/// discovery callback inline on the client loop and removes a discovery
/// operation only when the caller cancels it
/// (clientdiscover.cpp:83-112) — it never reinterprets a slow consumer
/// as cancellation. So we drop the event for a subscriber whose bounded
/// queue is momentarily full (lossy delivery, as `discover()` documents)
/// but KEEP the subscriber; a subscriber is removed only when its
/// receiver has been dropped (`Closed`). Treating `Full` as removal —
/// what a plain `try_send(..).is_ok()` retain does — silently unsubscribes
/// a live-but-slow consumer after a beacon storm, which pvxs never does.
fn publish_discovery(subscribers: &mut Vec<mpsc::Sender<Discovered>>, evt: Discovered) {
    subscribers.retain(|tx| match tx.try_send(evt.clone()) {
        Ok(()) => true,
        // Live consumer that fell behind: keep it; the event is lost, not
        // the subscription.
        Err(mpsc::error::TrySendError::Full(_)) => true,
        // Receiver dropped: the discovery operation is gone.
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    });
}

/// Emit the `Discovered` events implied by a beacon classification and
/// report whether pending searches should be poked. Mirrors pvxs
/// `onBeacon` (client.cpp:807-847): a `Change` emits `Timeout` for the old
/// GUID then `Online` for the new identity; a `New` emits `Online`; both
/// poke. `Update`/`CapDropped` emit nothing and do not poke. This is the
/// single place beacon identity transitions turn into discovery events.
fn emit_beacon_action(
    action: BeaconAction,
    server: SocketAddr,
    guid: [u8; 12],
    peer: SocketAddr,
    proto: String,
    subscribers: &mut Vec<mpsc::Sender<Discovered>>,
) -> bool {
    match action {
        BeaconAction::New => {
            publish_discovery(
                subscribers,
                Discovered::Online {
                    server,
                    guid,
                    peer,
                    proto,
                },
            );
            true
        }
        BeaconAction::Changed {
            old_guid,
            old_peer_version,
        } => {
            // pvxs emits a Timeout for the prior incarnation before the
            // Online for the new one (client.cpp:814-844). The Timeout
            // carries the SAME proto as the new beacon (this is one
            // `(server, proto)` identity changing GUID/version) and the
            // *old* peerVersion (pvxs `cur.peerVersion`); `proto` is cloned
            // because the Online below takes ownership of it.
            publish_discovery(
                subscribers,
                Discovered::Timeout {
                    server,
                    guid: old_guid,
                    proto: proto.clone(),
                    peer_version: old_peer_version,
                },
            );
            publish_discovery(
                subscribers,
                Discovered::Online {
                    server,
                    guid,
                    peer,
                    proto,
                },
            );
            true
        }
        BeaconAction::Update | BeaconAction::CapDropped => false,
    }
}

/// Returns bytes consumed from `bytes` so the caller can advance to
/// the next chained message in the same datagram.
/// `is_tcp`: true when the response arrived on a TCP name-server connection;
/// enables pvxs procSearchReply port-0 rule (client.cpp:828-846).
#[allow(clippy::too_many_arguments)]
fn handle_search_response(
    bytes: &[u8],
    pending: &mut HashMap<u32, Pending>,
    by_name: &mut HashMap<String, u32>,
    beacons: &Arc<BeaconTracker>,
    ignore_guids: &std::collections::HashSet<[u8; 12]>,
    subscribers: &mut Vec<mpsc::Sender<Discovered>>,
    poke_request: &mut bool,
    peer: SocketAddr,
    is_tcp: bool,
) -> usize {
    // Server-originated UDP — enforce direction bit (pvxs `conn.cpp:160`).
    let Ok(Some((frame, consumed))) = try_parse_frame_role(bytes, PeerRole::Client) else {
        return 0;
    };
    // A UDP SEARCH_RESPONSE must not be segmented — segment bits are a TCP
    // reassembly feature only, so pvxs drops a segmented UDP datagram
    // before processing (client.cpp:973-982). The same handler also serves
    // the TCP name-server stream (`is_tcp`), where segmentation is legal,
    // so the rejection is UDP-only.
    if !is_tcp && !frame.header.flags.unsegmented() {
        return consumed;
    }
    let Ok(resp) = decode_search_response(&frame) else {
        return consumed;
    };
    // pvxs client.cpp:889 — when a UDP SEARCH_RESPONSE arrives with
    // found=false and no channel IDs it is a discovery pong: the server
    // acknowledged a DiscoverPing (MustReply SEARCH with zero channels).
    // Treat it as a fake beacon so the server enters the tracker and
    // Discovered::Online fires for subscribers — mirrors pvxs's
    // `self.onBeacon(fakebeacon)` path. Pre-fix this returned unconditionally
    // on any found=false response, leaving DiscoverPing completely broken.
    //
    // The discovery-pong branch MUST only fire when a discovery is actually
    // outstanding. pvxs gates it on `!self.discoverers.empty()` (client.cpp:889);
    // the analog here is `!subscribers.is_empty()` (subscribers are the active
    // `discover()` operations, populated by SearchCommand::Subscribe). Without
    // this guard an ordinary not-found reply (any server that simply doesn't
    // host the searched PV, or a stale/duplicate reply) would spuriously feed
    // the beacon tracker, announce the server, and poke every pending search's
    // backoff — fabricating discovery activity nobody requested.
    //
    // pvxs additionally gates on `seq==search_seq` (client.cpp:889): the
    // discovery pong must echo the fixed `SEARCH_SEQ` we stamped into the
    // outgoing discovery SEARCH. A `found=false`/zero-cid reply carrying any
    // other sequence is some other server's SEARCH_RESPONSE, not a reply to
    // our DiscoverPing, and must NOT be promoted to a fake beacon.
    if !resp.found {
        if !subscribers.is_empty()
            && !is_tcp
            && resp.cids.is_empty()
            && resp.seq == SEARCH_SEQ
            && !ignore_guids.contains(&resp.guid)
        {
            let server = rewrite_loopback(resp.server_addr, peer);
            // pvxs converts the pong into a fake beacon and runs it through
            // onBeacon (client.cpp:889-899); the tracker classifies it and
            // emit_beacon_action produces Online (or Timeout+Online on a
            // changed identity). peerVersion is the reply frame's header
            // version, matching pvxs `peerVersion=head.version`.
            let action = beacons.observe(server, &resp.protocol, resp.guid, frame.header.version);
            let should_poke = emit_beacon_action(
                action,
                server,
                resp.guid,
                peer,
                resp.protocol.clone(),
                subscribers,
            );
            if should_poke {
                // pvxs `poke()` (client.cpp:736-759) only records `lastPoke`,
                // arms `nPoked`, and reschedules the search timer — it never
                // iterates channels or resets `nSearch`. The fast revolution's
                // poked ticks then keep each search's accumulated backoff
                // (`tickSearch(..., poked=true)` skips the `nSearch++`
                // increment, client.cpp:1141-1143, and requeues into the same
                // bucket). So this handler only signals the poke; the run
                // loop's single `maybe_poke()` + `rearm_after_send()` owner
                // decides cadence and retry. Resetting per-search backoff here
                // converted every pending search into a fresh first-retry,
                // amplifying UDP search load on a mass beacon/discovery event.
                *poke_request = true;
            }
        }
        return consumed;
    }
    // pvxs `client.cpp:872-904` drops any found SEARCH_RESPONSE whose
    // `proto != "tcp"`. The comparison is exact: pvxs's string decoder
    // can map a null/omitted marker to an empty `std::string`, but that
    // empty string still fails `proto != "tcp"` (pvaproto.h:392-405,
    // client.cpp:902-904) — there is no empty-protocol exception for
    // found=true channel resolution. The Rust UDP search engine only
    // opens plain TCP connections, so a server that did not advertise
    // exactly `"tcp"` (empty, a null marker, "tls", or an experimental
    // scheme) must be dropped rather than dialed on a transport it never
    // offered. The discovery-pong (found=false) path above is the only
    // place an empty/other protocol is tolerated, matching pvxs.
    if resp.protocol != "tcp" {
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
        // The full server identity decoded from this reply — pvxs stores
        // exactly this on the channel while Searching (client.cpp:925-927).
        let hit = SearchHit {
            server,
            guid: resp.guid,
            proto: resp.protocol.clone(),
            peer_version: frame.header.version,
        };
        let Some(entry) = pending.get_mut(&cid) else {
            continue;
        };
        match &mut entry.responder {
            Responder::Single(_) => {
                // Single responder: deliver the resolving hit and remove.
                // pvxs likewise moves the channel to Connecting on the first
                // reply (client.cpp:921-933); subsequent same-round replies
                // are only diagnostic. The channel captures `expected_guid`
                // from this hit, so a later reconnect that resolves to a
                // different GUID is what surfaces server replacement.
                let p = pending.remove(&cid).unwrap();
                by_name.remove(&p.pv_name);
                if let Responder::Single(tx) = p.responder {
                    let _ = tx.send(hit);
                }
            }
            Responder::Multi { accumulated, .. } => {
                // pvxs logs `Duplicate PV name %s from %s and %s` when two
                // servers with *different* GUIDs both claim one PV
                // (client.cpp:934-940). The multi-collector is the only
                // place that holds more than one reply for a name, so the
                // diagnostic lives here.
                if let Some(other) = accumulated.iter().find(|h| h.guid != hit.guid) {
                    tracing::error!(
                        pv = %entry.pv_name,
                        from = %other.server,
                        and = %hit.server,
                        "Duplicate PV name claimed by servers with different GUIDs"
                    );
                }
                if !accumulated.iter().any(|h| h.server == hit.server) {
                    accumulated.push(hit);
                }
                // Don't deliver yet — wait for the deadline tick to flush.
            }
        }
    }
    consumed
}

/// Returns bytes consumed from `bytes` so the caller can advance to
/// the next chained beacon in the same datagram.
fn handle_beacon(
    bytes: &[u8],
    beacons: &Arc<BeaconTracker>,
    subscribers: &mut Vec<mpsc::Sender<Discovered>>,
    poke_request: &mut bool,
    peer: SocketAddr,
) -> usize {
    // Beacons are server-originated — enforce direction bit
    // (pvxs `conn.cpp:160`).
    let Ok(Some((frame, consumed))) = try_parse_frame_role(bytes, PeerRole::Client) else {
        return 0;
    };
    // Beacons arrive only over UDP, which can carry neither control
    // messages nor segmentation; a segmented datagram is malformed and
    // dropped (pvxs udp_collector.cpp:329-340).
    if !frame.header.flags.unsegmented() {
        return consumed;
    }
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
    // pvxs decodes the BEACON protocol with `from_wire(M, beaconMsg.proto)`
    // and only invokes the beacon callbacks when the decode buffer is
    // still good afterward (udp_collector.cpp:478-488). pvxs's
    // `from_wire(Buffer&, std::string&)` faults the buffer ONLY when the
    // claimed length runs past the datagram (`!buf.ensure(len)`,
    // pvaproto.h:399-400); it copies the bytes verbatim with no UTF-8
    // validation otherwise (pvaproto.h:403). So a *truncated* protocol
    // string is the malformed case that must drop the beacon, whereas an
    // *invalid-UTF8 but length-complete* string is well-formed — pvxs
    // keeps M good and `onBeacon` announces it (the tracker keys on
    // (server, proto) with no `proto=="tcp"` filter, client.cpp:780).
    // `decode_string` mirrors this exactly: a short read is `Err` (drop);
    // a complete run decodes lossily (label path, PVA-89) and proceeds.
    // The empty/null-marker case stays good and decodes to "".
    let proto = match decode_string(&mut cur, order) {
        Ok(Some(p)) => p,
        Ok(None) => String::new(),
        Err(_) => return consumed,
    };
    let _status_size = decode_size(&mut cur, order).ok();

    let mut guid_arr = [0u8; 12];
    guid_arr.copy_from_slice(&guid);
    let mut addr_arr = [0u8; 16];
    addr_arr.copy_from_slice(&addr_bytes);
    // accept all-zero (IPv6 unspecified) too — pvxs
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

    // NOTE: pvxs `ignoreServerGUIDs` does NOT filter beacons. `onBeacon()`
    // (client.cpp:773-847) keys the tracker by (server, proto), fires
    // Discovered events, and pokes regardless of the ignore list; the list
    // is only consulted in procSearchReply (client.cpp:880). So a beacon
    // from an ignored GUID still flows through here — it just won't resolve
    // a searched channel when its SEARCH_RESPONSE is later dropped.
    //
    // peerVersion is the beacon frame's header version, matching pvxs
    // `beaconMsg.peerVersion = head.version` (udp_collector.cpp:465). The
    // tracker classifies New/Changed/Update keyed by (server, proto), so a
    // tcp and a tls beacon for the same server/GUID are distinct identities
    // and a peerVersion bump on the same GUID is a Change (client.cpp:807).
    let action = beacons.observe(server, &proto, guid_arr, frame.header.version);
    // pvxs poke()/event emission fires only on New or Change (not a steady
    // Update) — mirror of the SearchCommand::BeaconObserved path. Set the
    // poke_request flag so the main loop can flip the tick cadence to fast
    // (200 ms × 30) for one revolution.
    let should_poke = emit_beacon_action(action, server, guid_arr, peer, proto, subscribers);
    if should_poke {
        // pvxs `onBeacon()` routes a New/Changed identity through `poke()`
        // (client.cpp:773-847 → 736-759), which only records `lastPoke`,
        // arms `nPoked`, and reschedules the search timer. It does NOT
        // touch any channel's `nSearch`. The fast revolution then retransmits
        // every parked search while preserving its accumulated backoff: the
        // poked tick skips the `nSearch++` increment and requeues into the
        // same bucket (client.cpp:1141-1143). So this handler only signals
        // the poke; the run loop's single `maybe_poke()` + `rearm_after_send()`
        // owner decides cadence and retry. Resetting per-search backoff here
        // turned every pending search into a fresh first-retry, amplifying
        // UDP search broadcast on a mass beacon event.
        *poke_request = true;
    }
    consumed
}

fn rewrite_loopback(addr: SocketAddr, peer: SocketAddr) -> SocketAddr {
    // pvxs `procSearchReply` (client.cpp:851-870) substitutes the
    // datagram source address ONLY when the advertised server address is
    // wildcard/unspecified (`serv.isAny()`). An explicit address is taken
    // verbatim — there is no `is_loopback()` rewrite upstream. Rewriting
    // an explicit `127.0.0.1` / `::1` would turn a deliberately
    // loopback-only advertisement into a remote connection attempt that
    // pvxs never makes, and would diverge the chosen endpoint on hosts
    // where the peer also listens on the rewritten address.
    if !addr.ip().is_unspecified() {
        return addr;
    }
    if !peer.ip().is_loopback() {
        SocketAddr::new(peer.ip(), addr.port())
    } else {
        // PR #205 IPv6 Stage 4: a wildcard advertisement from a loopback
        // peer resolves to loopback of the peer's family rather than a
        // hard-coded `127.0.0.1`, so a v6 SEARCH from `[::1]` targets the
        // v6 listener.
        let lo = match peer.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        SocketAddr::new(lo, addr.port())
    }
}

// ── TCP name-server tasks ───────────────────────────────────────────────

/// Engine-side handle for one TCP name-server connection.
///
/// `ready` is the per-NS readiness gate owned by [`ns_task`]: it is `false`
/// until the connection reaches CONNECTION_VALIDATED and flips back to `false`
/// the instant the connection drops. The engine consults it before forwarding
/// a SEARCH so frames are never buffered across a disconnected/handshaking NS.
struct NsHandle {
    tx: mpsc::Sender<Vec<u8>>,
    ready: Arc<AtomicBool>,
}

/// Forward the current tick's SEARCH for `sid`/`pv_name` to every name-server
/// connection that is currently *ready* (validated and connected).
///
/// pvxs SEARCHes over each NS connection (client.cpp:1193-1196) but skips any
/// name server unless `serv->ready && serv->connection()` are both true
/// (client.cpp:1221-1225), and keeps no disconnected-side queue that is replayed
/// after reconnect (client.cpp:1227-1235). A not-ready NS is simply skipped for
/// this tick; the search is retried on a later tick by the normal bucket cadence
/// once the NS is ready, instead of replaying a stale backlog. The unicast bit
/// is set and port=0 so the NS replies on the same TCP connection.
/// Pack `(search_id, pv_name)` entries into one or more SEARCH frames,
/// each kept under [`MAX_SEARCH_PAYLOAD`]. Mirrors pvxs `tickSearch`
/// (`client.cpp:1083-1101`): names accumulate into the current packet
/// and a name that would push the packet past the limit is deferred to
/// the next one; a single name that alone exceeds the limit is sent on
/// its own (no choice but to fragment). The channel count is emitted
/// once per frame by [`PvaCodec::build_search_batch`], so N channels
/// cost `ceil(total_bytes / MAX_SEARCH_PAYLOAD)` datagrams instead of N.
/// `response_port`/`unicast` select the broadcast shape
/// (`port=response_port`, `unicast=false`) or the name-server shape
/// (`port=0`, `unicast=true`).
fn pack_search_frames(
    codec: &PvaCodec,
    entries: &[(u32, String)],
    response_port: u16,
    unicast: bool,
) -> Vec<Vec<u8>> {
    let build = |batch: &[(u32, &str)]| {
        // pvxs stamps the fixed `search_seq` ("find") into EVERY SEARCH,
        // not just discovery (client.cpp:1072, shared `tickSearch` loop);
        // the searchSequenceID is redundant but must match the contract.
        codec.build_search_batch(SEARCH_SEQ, batch, [0, 0, 0, 0], response_port, unicast)
    };
    let mut frames = Vec::new();
    let mut batch: Vec<(u32, &str)> = Vec::new();
    for (cid, name) in entries {
        batch.push((*cid, name.as_str()));
        if build(&batch).len() > MAX_SEARCH_PAYLOAD && batch.len() > 1 {
            // This entry overflowed the datagram: emit the batch
            // without it, then start a fresh batch carrying it.
            batch.pop();
            frames.push(build(&batch));
            batch.clear();
            batch.push((*cid, name.as_str()));
        }
    }
    if !batch.is_empty() {
        frames.push(build(&batch));
    }
    frames
}

/// Forward already-packed SEARCH frames to every *ready* name-server
/// connection. pvxs reuses the same packed `searchMsg` for name-server
/// forwarding after zeroing the UDP response port (`client.cpp:1217-1234`);
/// these frames are already built with `port=0` + the unicast bit. A
/// not-ready NS is skipped this tick (no replayed backlog). The bounded
/// channel's `Full` error drops the frame rather than growing an
/// unbounded backlog (pvxs `client.cpp:1227-1235`).
fn ns_forward_frames(handles: &[NsHandle], frames: &[Vec<u8>]) {
    if frames.is_empty() || handles.iter().all(|h| !h.ready.load(Ordering::SeqCst)) {
        return;
    }
    for ns in handles {
        if ns.ready.load(Ordering::SeqCst) {
            for frame in frames {
                let _ = ns.tx.try_send(frame.clone());
            }
        }
    }
}

/// Long-running task for one EPICS_PVA_NAME_SERVERS entry.
/// Loops forever: connect, handshake, forward SEARCHes / receive responses,
/// reconnect after 10 s on any failure. pvxs client.cpp:651-667 + 1295-1305.
async fn ns_task(
    ns_addr: SocketAddr,
    mut search_rx: mpsc::Receiver<Vec<u8>>,
    response_tx: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    ready: Arc<AtomicBool>,
    user: String,
    host: String,
    handshake_timeout: Duration,
) {
    // pvxs client.cpp:68: tcpNSCheckInterval = 10s.
    const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
    loop {
        let res = ns_run_once(
            ns_addr,
            &mut search_rx,
            &response_tx,
            &ready,
            &user,
            &host,
            handshake_timeout,
        )
        .await;
        // Connection is gone: clear readiness immediately so the engine skips
        // this NS for every tick until the next CONNECTION_VALIDATED, rather
        // than enqueuing frames into a channel nobody is draining.
        ready.store(false, Ordering::SeqCst);
        if let Err(e) = res {
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
    ready: &AtomicBool,
    user: &str,
    host: &str,
    handshake_timeout: Duration,
) -> std::io::Result<()> {
    let stream = TcpStream::connect(ns_addr).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let mut rx_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // CONNECTION_VALIDATION handshake. pvxs builds name-server peers with the
    // same Connection::build()/auth negotiation as ordinary TCP servers and
    // only flips `nameserver = true` afterward (client.cpp:674-685); there is
    // no name-server auth exception (clientconn.cpp:215-263). Reuse the normal
    // client handshake helpers so the NS sees the same "ca"-preferred user/host
    // identity a regular server connection presents, instead of forced anonymous.
    // `None` message cap = pvxs-parity unbounded; the read is deadlined by
    // `handshake_timeout`.
    let (byte_order, _server_buf, _server_reg, auth_methods) =
        read_handshake_init(&mut reader, &mut rx_buf, handshake_timeout, None)
            .await
            .map_err(std::io::Error::other)?;
    let negotiated_auth = select_client_auth(&auth_methods);
    let reply = build_client_connection_validation(
        byte_order,
        DEFAULT_BUFFER_SIZE,
        DEFAULT_REGISTRY_SIZE,
        0,
        negotiated_auth,
        user,
        host,
    );
    writer.write_all(&reply).await?;
    wait_for_validated(&mut reader, &mut rx_buf, handshake_timeout, None)
        .await
        .map_err(std::io::Error::other)?;

    // Discard any SEARCH frames that the engine enqueued while this connection
    // was down or handshaking. pvxs keeps no disconnected-side queue to replay
    // (client.cpp:1227-1235); the still-pending searches are re-sent on the next
    // ready tick by the normal bucket cadence, so replaying a stale backlog would
    // only generate load for CIDs whose callers may already be gone. Only after
    // draining do we publish readiness, so the engine never races a frame into
    // the about-to-be-drained window.
    while search_rx.try_recv().is_ok() {}
    ready.store(true, Ordering::SeqCst);

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

#[allow(dead_code)]
fn _suppress(_: PvaHeader) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ByteOrder, ControlCommand, WriteExt, encode_string_into};
    use serial_test::serial;

    /// Regression R0604-PVASRV-MCAST-JOIN-IFACE-1 (client half, same
    /// `@iface`-dropping defect family as the server beacon-join path): the
    /// `EPICS_PVA_ADDR_LIST` multicast projection must keep the `@iface`
    /// modifier so an explicit-interface group is joined on that interface
    /// alone (`Some(ip)`), while a modifier-less group stays an all-NIC join
    /// (`None`). The pre-fix code projected every group to the all-NIC
    /// `join_multicast_v4`, dropping the interface entirely. IP-literal
    /// `@iface` forms make `resolve_iface_v4` a pure passthrough.
    #[test]
    fn addr_list_multicast_targets_preserves_iface_modifier() {
        let modifierless: crate::config::Endpoint =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 4, 4)), 5076).into();
        let explicit = crate::config::Endpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 5, 5)), 5076),
            ttl: None,
            iface: Some("192.168.7.7".to_string()),
        };
        // Unicast and v6 entries are dropped.
        let unicast: crate::config::Endpoint =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 5076).into();
        let v6: crate::config::Endpoint = SocketAddr::V6(std::net::SocketAddrV6::new(
            Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x400),
            5076,
            0,
            0,
        ))
        .into();

        assert_eq!(
            addr_list_multicast_targets(&[modifierless, explicit, unicast, v6]),
            vec![
                (Ipv4Addr::new(224, 0, 4, 4), None),
                (
                    Ipv4Addr::new(224, 0, 5, 5),
                    Some(Ipv4Addr::new(192, 168, 7, 7))
                ),
            ],
            "modifier-less group → all-NIC (None); @iface group → that interface (Some)"
        );
    }

    // ---- name-server dedup (pvxs split_addr_into sort+unique) ----
    //
    // Regression R0604-PVA-NAMESERVER-DEDUP-1: a duplicate
    // `EPICS_PVA_NAME_SERVERS` token must collapse to one connection target,
    // matching pvxs `split_addr_into` (config.cpp:179-183). One case per
    // boundary: an exact duplicate is dropped; distinct targets are kept in
    // first-seen order (not sorted); an already-unique list is unchanged.
    #[test]
    fn dedup_name_servers_collapses_duplicates_preserving_order() {
        let a: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let c: SocketAddr = "10.0.0.1:5075".parse().unwrap();

        // Exact duplicate token → one entry.
        assert_eq!(dedup_name_servers(vec![a, a]), vec![a]);

        // Distinct targets preserved in first-seen order (pvxs sorts, but the
        // sort is unobservable since name servers are queried in parallel).
        assert_eq!(dedup_name_servers(vec![c, a, b]), vec![c, a, b]);

        // Interleaved duplicates: keep the first occurrence of each.
        assert_eq!(dedup_name_servers(vec![a, b, a, c, b]), vec![a, b, c]);

        // Already-unique list is returned unchanged.
        assert_eq!(dedup_name_servers(vec![a, b, c]), vec![a, b, c]);

        // Empty list stays empty.
        assert_eq!(dedup_name_servers(Vec::new()), Vec::<SocketAddr>::new());
    }

    // ---- maybe_poke rate-limit invariant (pvxs pokeHoldoff, A8-R2-1) ----
    //
    // One case per invariant boundary of the single poke gate, asserting
    // both the grant decision and the resulting state, so a regression in
    // either guard (skip-while-active, 30 s holdoff) is caught. These are
    // unit tests of the gate; the end-to-end fast-tick behaviour is covered
    // by `hurry_up_kicks_pending_searches_at_fast_tick_cadence`.

    #[tokio::test(flavor = "current_thread")]
    async fn maybe_poke_first_poke_granted_arms_revolution() {
        // last_poke == None, no active revolution → granted.
        let mut tick = interval(Duration::from_secs(1));
        let mut ftr = 0u32;
        let mut last = None;
        assert!(maybe_poke(&mut tick, &mut ftr, &mut last).await);
        assert_eq!(ftr, N_SEARCH_BUCKETS as u32);
        assert!(last.is_some(), "granted poke must record a poke time");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maybe_poke_declines_while_revolution_active() {
        // fast_ticks_remaining > 0 → skip-while-active, no re-arm/extend.
        let mut tick = interval(Duration::from_secs(1));
        let mut ftr = 5u32;
        let mut last = None;
        assert!(!maybe_poke(&mut tick, &mut ftr, &mut last).await);
        assert_eq!(ftr, 5, "active revolution must not be re-armed or extended");
        assert!(last.is_none(), "declined poke must not record a poke time");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maybe_poke_declines_within_holdoff_window() {
        // last_poke just now, no active revolution → inside 30 s holdoff.
        let mut tick = interval(Duration::from_secs(1));
        let mut ftr = 0u32;
        let mut last = Some(Instant::now());
        assert!(!maybe_poke(&mut tick, &mut ftr, &mut last).await);
        assert_eq!(
            ftr, 0,
            "poke inside the holdoff window must not arm a revolution"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maybe_poke_granted_once_holdoff_elapsed() {
        // last_poke older than POKE_HOLDOFF, no active revolution → granted.
        let mut tick = interval(Duration::from_secs(1));
        let mut ftr = 0u32;
        let prev = Instant::now() - (POKE_HOLDOFF + Duration::from_secs(1));
        let mut last = Some(prev);
        assert!(maybe_poke(&mut tick, &mut ftr, &mut last).await);
        assert_eq!(ftr, N_SEARCH_BUCKETS as u32);
        assert!(last.unwrap() > prev, "granted poke must advance last_poke");
    }

    /// RAII guard for the process-global `EPICS_PVA_*` env vars the
    /// tests below set to pin search behaviour (suppress real broadcast
    /// fan-out, fix the beacon port). It snapshots each key's prior
    /// value on construction and restores it on drop — so a panicking
    /// assertion can never leak a mutated var into a sibling test that
    /// shares this process.
    ///
    /// `nextest` isolates each test in its own process, but `cargo test`
    /// runs them as threads in one process — there the leak is real,
    /// which is why every env-mutating test here also carries
    /// `#[serial(epics_env)]` (the same cross-crate group key used by
    /// the `auth::tls` and `epics-base-rs` net tests) so no two of them
    /// mutate the shared environment concurrently. The earlier
    /// `flavor = "current_thread"` "SAFETY" rationale was wrong: that
    /// flavor only constrains the test's own async executor, not the
    /// test harness's cross-test thread parallelism.
    struct EnvVarGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvVarGuard {
        /// Snapshot then set each `(key, value)`; restored on drop.
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let saved = vars
                .iter()
                .map(|(k, _)| (*k, std::env::var(k).ok()))
                .collect();
            // SAFETY: only constructed inside `#[serial(epics_env)]`
            // tests, so no other thread reads or writes these keys
            // concurrently — the reason `set_var` is `unsafe` in the
            // 2024 edition.
            unsafe {
                for (k, v) in vars {
                    std::env::set_var(k, v);
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: same serialization guarantee as `set`.
            unsafe {
                for (k, prev) in &self.saved {
                    match prev {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

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

    // PVA-RS-2026-05-28-123: pvxs `procSearchReply` rewrites ONLY
    // `isAny()` (wildcard/unspecified) — never an explicit address. An
    // explicit `127.0.0.1` advertised in a SEARCH_RESPONSE that arrives
    // from a non-loopback peer must be PRESERVED, not rewritten to the
    // packet source; rewriting it would dial a remote endpoint pvxs
    // would not. (This previously asserted the opposite, divergent
    // behavior.)
    #[test]
    fn rewrite_loopback_explicit_loopback_preserved_from_remote_peer() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(
            r, a,
            "an explicit loopback advertisement must be used verbatim (pvxs parity)"
        );
    }

    #[test]
    fn rewrite_loopback_explicit_loopback_kept_for_loopback_peer() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5076);
        let r = rewrite_loopback(a, peer);
        assert_eq!(r, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075));
    }

    /// Build a server→client `SEARCH_RESPONSE` frame with `found=false`
    /// and zero channel IDs — the on-wire shape of a discovery pong
    /// (the reply to a `DiscoverPing`). The sequence echoes the fixed
    /// `SEARCH_SEQ` we stamp into the discovery SEARCH, which the pong
    /// path requires (pvxs client.cpp:889).
    fn found_false_response() -> Vec<u8> {
        crate::server_native::udp::build_search_response_proto(
            [0x42u8; 12],
            SEARCH_SEQ,
            5075,
            &[], // empty cids ⇒ found byte encodes false
            ByteOrder::Little,
            "tcp",
        )
    }

    /// Regression: a `found=false` UDP `SEARCH_RESPONSE` must NOT be
    /// treated as a discovery pong when no `discover()` is outstanding.
    /// pvxs gates the fake-beacon branch on `!self.discoverers.empty()`
    /// (client.cpp:889); the analog here is `!subscribers.is_empty()`.
    /// Without an active subscriber the reply must be inert — no beacon
    /// observation, no announce, no poke.
    #[test]
    fn discovery_pong_inert_without_active_discover() {
        let frame = found_false_response();
        let beacons = BeaconTracker::new();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        let mut by_name: HashMap<String, u32> = HashMap::new();
        let ignore_guids: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let mut subscribers: Vec<mpsc::Sender<Discovered>> = Vec::new(); // no discover() active
        let mut poke = false;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);

        let consumed = handle_search_response(
            &frame,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore_guids,
            &mut subscribers,
            &mut poke,
            peer,
            false, // UDP
        );

        assert!(consumed > 0, "frame must still be consumed/advanced");
        assert!(
            !poke,
            "no discover() outstanding ⇒ discovery-pong poke must NOT fire"
        );
        // No subscriber ⇒ the pong never reaches the tracker.
        let server = SocketAddr::new(peer.ip(), 5075);
        assert!(
            beacons.guid_for(server).is_none(),
            "no discover() outstanding ⇒ server must NOT enter the beacon tracker"
        );
    }

    /// Discovery must still work: with an active `discover()` subscriber, a
    /// `found=false`/zero-cid UDP reply IS a discovery pong — fake-beacon
    /// processing fires (announce + poke) and `Discovered::Online` is
    /// delivered to the subscriber.
    #[test]
    fn discovery_pong_fires_with_active_discover() {
        let frame = found_false_response();
        let beacons = BeaconTracker::new();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        let mut by_name: HashMap<String, u32> = HashMap::new();
        let ignore_guids: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let (tx, mut rx) = mpsc::channel::<Discovered>(8);
        let mut subscribers: Vec<mpsc::Sender<Discovered>> = vec![tx]; // discover() active
        let mut poke = false;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);

        let consumed = handle_search_response(
            &frame,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore_guids,
            &mut subscribers,
            &mut poke,
            peer,
            false, // UDP
        );

        assert!(consumed > 0, "frame must be consumed");
        assert!(
            poke,
            "fresh server via discovery pong must poke pending searches"
        );
        // The fake beacon enters the tracker (the de-dup owner) exactly once.
        let server = SocketAddr::new(peer.ip(), 5075);
        assert_eq!(
            beacons.guid_for(server),
            Some([0x42u8; 12]),
            "server must be tracked exactly once via the fake-beacon path"
        );
        match rx.try_recv() {
            Ok(Discovered::Online { guid, .. }) => assert_eq!(guid, [0x42u8; 12]),
            other => panic!("expected Discovered::Online for discovery pong, got {other:?}"),
        }
    }

    /// pvxs gates the discovery-pong fake-beacon branch on
    /// `seq==search_seq` (client.cpp:889). A `found=false`/zero-cid UDP
    /// reply whose sequence is NOT our `SEARCH_SEQ` is some other
    /// server's not-found reply, not a reply to our DiscoverPing, and
    /// must not be promoted to a fake beacon — even with an active
    /// `discover()` subscriber.
    #[test]
    fn discovery_pong_rejected_on_wrong_sequence() {
        // Same shape as found_false_response() but with a stray sequence.
        let frame = crate::server_native::udp::build_search_response_proto(
            [0x42u8; 12],
            SEARCH_SEQ ^ 0x1,
            5075,
            &[],
            ByteOrder::Little,
            "tcp",
        );
        let beacons = BeaconTracker::new();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        let mut by_name: HashMap<String, u32> = HashMap::new();
        let ignore_guids: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let (tx, mut rx) = mpsc::channel::<Discovered>(8);
        let mut subscribers: Vec<mpsc::Sender<Discovered>> = vec![tx]; // discover() active
        let mut poke = false;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);

        let consumed = handle_search_response(
            &frame,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore_guids,
            &mut subscribers,
            &mut poke,
            peer,
            false,
        );

        assert!(consumed > 0, "frame must still be consumed/advanced");
        assert!(!poke, "wrong-sequence reply must not poke");
        let server = SocketAddr::new(peer.ip(), 5075);
        assert!(
            beacons.guid_for(server).is_none(),
            "wrong-sequence found=false reply must NOT be promoted to a fake beacon"
        );
        assert!(
            rx.try_recv().is_err(),
            "no Discovered event for a non-matching sequence"
        );
    }

    /// `ignore_server_guids` is a SEARCH_RESPONSE-only filter, matching
    /// pvxs (consulted in procSearchReply client.cpp:880, never in
    /// onBeacon client.cpp:773-847). A found SEARCH_RESPONSE and a
    /// discovery pong from an ignored GUID are dropped, but a BEACON from
    /// the same GUID still announces the server and fires
    /// `Discovered::Online`.
    #[test]
    fn ignore_guids_drops_search_replies_not_beacons() {
        use tokio::sync::oneshot;
        const IG: [u8; 12] = [0x42u8; 12];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);
        let ignore: std::collections::HashSet<[u8; 12]> = std::iter::once(IG).collect();

        // (1) found=true SEARCH_RESPONSE with an ignored GUID must NOT
        // resolve the pending channel.
        let (tx, _rx) = oneshot::channel::<SearchHit>();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        pending.insert(
            1,
            Pending {
                pv_name: "dut".into(),
                responder: Responder::Single(tx),
                last_attempt: Instant::now(),
                attempt: 1,
                bucket: 0,
            },
        );
        let mut by_name: HashMap<String, u32> = std::iter::once(("dut".to_string(), 1)).collect();
        let beacons = BeaconTracker::new();
        let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
        let mut poke = false;
        let found_true = crate::server_native::udp::build_search_response_proto(
            IG,
            0,
            5075,
            &[1],
            ByteOrder::Little,
            "tcp",
        );
        handle_search_response(
            &found_true,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs,
            &mut poke,
            peer,
            false,
        );
        assert!(
            pending.contains_key(&1),
            "ignored-GUID found SEARCH_RESPONSE must be dropped, not resolved"
        );

        // (2) found=false discovery pong with an ignored GUID, even with an
        // active discover() subscriber, must NOT announce.
        let (txd, mut rxd) = mpsc::channel::<Discovered>(8);
        let mut subs2: Vec<mpsc::Sender<Discovered>> = vec![txd];
        let mut poke2 = false;
        let pong = crate::server_native::udp::build_search_response_proto(
            IG,
            SEARCH_SEQ,
            5075,
            &[],
            ByteOrder::Little,
            "tcp",
        );
        handle_search_response(
            &pong,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs2,
            &mut poke2,
            peer,
            false,
        );
        let server = SocketAddr::new(peer.ip(), 5075);
        assert!(
            beacons.guid_for(server).is_none(),
            "ignored-GUID discovery pong must be dropped, not tracked"
        );
        assert!(
            rxd.try_recv().is_err(),
            "ignored-GUID discovery pong must emit no Discovered event"
        );

        // (3) BEACON with the ignored GUID still announces + emits Online.
        // handle_beacon takes no ignore set — the filter cannot reach it.
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.extend_from_slice(&IG); // guid
        payload.put_u8(0); // flags
        payload.put_u8(0); // seq
        payload.put_u16(0, order); // change
        payload.extend_from_slice(&[0u8; 16]); // addr wildcard → peer
        payload.put_u16(5075, order); // port
        encode_string_into("tcp", order, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
        let mut frame = Vec::new();
        header.write_into(&mut frame);
        frame.extend_from_slice(&payload);

        let (txb, mut rxb) = mpsc::channel::<Discovered>(8);
        let mut subs3: Vec<mpsc::Sender<Discovered>> = vec![txb];
        let mut poke3 = false;
        handle_beacon(&frame, &beacons, &mut subs3, &mut poke3, peer);
        assert_eq!(
            beacons.guid_for(server),
            Some(IG),
            "ignored-GUID BEACON must still track the server (pvxs onBeacon has no ignore check)"
        );
        assert!(
            matches!(rxb.try_recv(), Ok(Discovered::Online { guid, .. }) if guid == IG),
            "ignored-GUID BEACON must still emit Discovered::Online"
        );
    }

    /// Segment bits are a TCP reassembly feature; a UDP SEARCH_RESPONSE
    /// must never carry them. pvxs drops a segmented UDP datagram before
    /// processing (client.cpp:973-982). A segmented found=true reply must
    /// NOT resolve the pending channel, while the same frame un-segmented
    /// does — proving the rejection is the segment bits, not the payload.
    #[test]
    fn segmented_udp_search_response_does_not_resolve() {
        use crate::proto::header::HeaderFlags;
        use tokio::sync::oneshot;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);
        let base = crate::server_native::udp::build_search_response_proto(
            [0x42u8; 12],
            0,
            5075,
            &[1],
            ByteOrder::Little,
            "tcp",
        );

        // Segmented (any segment bit) UDP reply: dropped, pending kept.
        for seg in [
            HeaderFlags::SEGMENT_FIRST,
            HeaderFlags::SEGMENT_LAST,
            HeaderFlags::SEGMENT_MASK,
        ] {
            let mut frame = base.clone();
            frame[2] |= seg; // flags byte
            let (tx, _rx) = oneshot::channel::<SearchHit>();
            let mut pending: HashMap<u32, Pending> = HashMap::new();
            pending.insert(
                1,
                Pending {
                    pv_name: "dut".into(),
                    responder: Responder::Single(tx),
                    last_attempt: Instant::now(),
                    attempt: 1,
                    bucket: 0,
                },
            );
            let mut by_name: HashMap<String, u32> =
                std::iter::once(("dut".to_string(), 1)).collect();
            let beacons = BeaconTracker::new();
            let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
            let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
            let mut poke = false;
            let consumed = handle_search_response(
                &frame,
                &mut pending,
                &mut by_name,
                &beacons,
                &ignore,
                &mut subs,
                &mut poke,
                peer,
                false, // UDP
            );
            assert!(consumed > 0, "segmented frame must still be advanced");
            assert!(
                pending.contains_key(&1),
                "segmented UDP SEARCH_RESPONSE (bits {seg:#04x}) must not resolve the pending channel"
            );
        }

        // Control: the same frame un-segmented DOES resolve.
        let (tx, mut rx) = oneshot::channel::<SearchHit>();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        pending.insert(
            1,
            Pending {
                pv_name: "dut".into(),
                responder: Responder::Single(tx),
                last_attempt: Instant::now(),
                attempt: 1,
                bucket: 0,
            },
        );
        let mut by_name: HashMap<String, u32> = std::iter::once(("dut".to_string(), 1)).collect();
        let beacons = BeaconTracker::new();
        let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
        let mut poke = false;
        handle_search_response(
            &base,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs,
            &mut poke,
            peer,
            false,
        );
        assert!(
            !pending.contains_key(&1),
            "un-segmented found SEARCH_RESPONSE must resolve the pending channel"
        );
        assert!(
            rx.try_recv().is_ok(),
            "resolved channel must receive a server addr"
        );
    }

    /// Regression R0604-PVACLI-SEARCH-GUID-DISCARD-1 (point 1): a found=true
    /// `SEARCH_RESPONSE` resolves the single-responder channel with the GUID
    /// decoded from *that reply*, even when no beacon has ever been observed
    /// for the server. pvxs stores `chan->guid = guid` straight off the reply
    /// (client.cpp:925-927); the Rust engine hands the resolving `SearchHit`
    /// (carrying `resp.guid`) to `find()` so the channel's `expected_guid`
    /// comes from the search reply, not a (possibly absent) beacon.
    ///
    /// FAIL-proof: structural. Before the fix `Responder::Single` carried a
    /// bare `oneshot::Sender<SocketAddr>`, so the reply GUID never left the
    /// engine and this assertion could not be written (no `.guid` on the
    /// delivered value). Reverting the hit threading reverts the responder
    /// type and breaks compilation.
    #[test]
    fn found_reply_delivers_search_hit_guid_without_beacon() {
        use tokio::sync::oneshot;
        const G: [u8; 12] = [0xABu8; 12];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 60)), 5076);
        let server = SocketAddr::new(peer.ip(), 5075);

        let (tx, mut rx) = oneshot::channel::<SearchHit>();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        pending.insert(
            1,
            Pending {
                pv_name: "dut".into(),
                responder: Responder::Single(tx),
                last_attempt: Instant::now(),
                attempt: 1,
                bucket: 0,
            },
        );
        let mut by_name: HashMap<String, u32> = std::iter::once(("dut".to_string(), 1)).collect();
        // Empty tracker: no beacon has ever been seen for this server.
        let beacons = BeaconTracker::new();
        assert!(
            beacons.guid_for(server).is_none(),
            "precondition: no beacon GUID for the server"
        );
        let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
        let mut poke = false;
        let frame = crate::server_native::udp::build_search_response_proto(
            G,
            0,
            5075,
            &[1],
            ByteOrder::Little,
            "tcp",
        );
        handle_search_response(
            &frame,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs,
            &mut poke,
            peer,
            false, // UDP
        );
        let hit = rx
            .try_recv()
            .expect("found=true reply must resolve the single responder");
        assert_eq!(
            hit.guid, G,
            "resolving SearchHit must carry the SEARCH_RESPONSE GUID, \
             not a beacon-derived one"
        );
        assert_eq!(hit.proto, "tcp");
        assert_eq!(hit.server, server);
    }

    /// Regression R0604-PVACLI-SEARCH-GUID-DISCARD-1 (point 3): a transient
    /// `tls` beacon recorded for the *same server address* with a different
    /// GUID must NOT influence the GUID a `tcp` `SEARCH_RESPONSE` resolves
    /// with. The beacon tracker is keyed by `(server, proto)` and
    /// `guid_for(addr)` collapses by address alone (the 4912f4bc hazard), so
    /// before the fix the channel re-derived `expected_guid` from
    /// `last_guid_for(addr)` and could pick up the tls beacon's GUID. The
    /// resolving `SearchHit` now carries the reply's own GUID, and the channel
    /// sets `expected_guid: cand.guid.or_else(|| last_guid_for(..))` — so with
    /// a present hit GUID the beacon fallback is never consulted.
    ///
    /// FAIL-proof: structural, as above — the reply GUID flows through
    /// `SearchHit`; reverting breaks the type. The `guid_for` precondition
    /// documents the contrast: address-only lookup yields the *tls* GUID.
    #[test]
    fn found_tcp_reply_guid_unaffected_by_tls_beacon_same_addr() {
        use tokio::sync::oneshot;
        const G_TLS: [u8; 12] = [0x11u8; 12];
        const G_TCP: [u8; 12] = [0x22u8; 12];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 70)), 5076);
        let server = SocketAddr::new(peer.ip(), 5075);

        let beacons = BeaconTracker::new();
        // A tls beacon for the same endpoint carries a different transient GUID.
        beacons.observe(server, "tls", G_TLS, 2);
        assert_eq!(
            beacons.guid_for(server),
            Some(G_TLS),
            "precondition: address-only beacon lookup yields the tls GUID — \
             the wrong source the fix must bypass"
        );

        let (tx, mut rx) = oneshot::channel::<SearchHit>();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        pending.insert(
            1,
            Pending {
                pv_name: "dut".into(),
                responder: Responder::Single(tx),
                last_attempt: Instant::now(),
                attempt: 1,
                bucket: 0,
            },
        );
        let mut by_name: HashMap<String, u32> = std::iter::once(("dut".to_string(), 1)).collect();
        let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
        let mut poke = false;
        let frame = crate::server_native::udp::build_search_response_proto(
            G_TCP,
            0,
            5075,
            &[1],
            ByteOrder::Little,
            "tcp",
        );
        handle_search_response(
            &frame,
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs,
            &mut poke,
            peer,
            false,
        );
        let hit = rx.try_recv().expect("tcp reply must resolve the channel");
        assert_eq!(
            hit.guid, G_TCP,
            "channel GUID must come from the tcp SEARCH_RESPONSE, \
             not the tls beacon at the same address"
        );
        assert_ne!(hit.guid, G_TLS);
    }

    /// Regression R0604-PVACLI-SEARCH-GUID-DISCARD-1 (point 2): when two
    /// servers with *different* GUIDs both claim one PV name, the multi-server
    /// collector logs the pvxs duplicate-PV diagnostic (`procSearchReply`,
    /// client.cpp:934-940) instead of silently dropping the second reply. The
    /// first reply seeds `accumulated`; the second with a mismatched GUID must
    /// emit an ERROR-level event.
    ///
    /// FAIL-proof: removing the
    /// `accumulated.iter().find(|h| h.guid != hit.guid)` diagnostic block in
    /// `handle_search_response` drops the ERROR event and fails this test.
    #[test]
    fn duplicate_pv_different_guid_logs_diagnostic() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::oneshot;
        use tracing::Level;
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::prelude::*;

        struct LevelCapture(Arc<Mutex<Vec<Level>>>);
        impl<S: tracing::Subscriber> Layer<S> for LevelCapture {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                self.0.lock().unwrap().push(*event.metadata().level());
            }
        }

        const G_A: [u8; 12] = [0xA1u8; 12];
        const G_B: [u8; 12] = [0xB2u8; 12];
        let peer_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5076);
        let peer_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 5076);

        let levels = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(LevelCapture(levels.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let (tx, _rx) = oneshot::channel::<Vec<SearchHit>>();
            // First reply (GUID A from server A) already accumulated.
            let seed = SearchHit {
                server: SocketAddr::new(peer_a.ip(), 5075),
                guid: G_A,
                proto: "tcp".into(),
                peer_version: 2,
            };
            let mut pending: HashMap<u32, Pending> = HashMap::new();
            pending.insert(
                1,
                Pending {
                    pv_name: "dut".into(),
                    responder: Responder::Multi {
                        responder: tx,
                        accumulated: vec![seed],
                        deadline: Instant::now(),
                    },
                    last_attempt: Instant::now(),
                    attempt: 1,
                    bucket: 0,
                },
            );
            let mut by_name: HashMap<String, u32> =
                std::iter::once(("dut".to_string(), 1)).collect();
            let beacons = BeaconTracker::new();
            let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
            let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
            let mut poke = false;
            // Second reply: same cid, different GUID B, from server B.
            let frame = crate::server_native::udp::build_search_response_proto(
                G_B,
                0,
                5075,
                &[1],
                ByteOrder::Little,
                "tcp",
            );
            handle_search_response(
                &frame,
                &mut pending,
                &mut by_name,
                &beacons,
                &ignore,
                &mut subs,
                &mut poke,
                peer_b,
                false,
            );
        });
        let got = levels.lock().unwrap().clone();
        assert!(
            got.contains(&Level::ERROR),
            "a second found reply with a different GUID must emit the pvxs \
             duplicate-PV ERROR diagnostic; captured levels: {got:?}"
        );
    }

    /// A segmented UDP BEACON datagram must be dropped — segment bits are
    /// TCP-only (pvxs udp_collector.cpp:329-340) — producing no discovery
    /// event and no tracker entry, while the un-segmented beacon does.
    #[test]
    fn segmented_udp_beacon_emits_no_event() {
        use crate::proto::header::HeaderFlags;
        const G: [u8; 12] = [0x77u8; 12];
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);
        let server = SocketAddr::new(peer.ip(), 5075);
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.extend_from_slice(&G); // guid
        payload.put_u8(0); // flags
        payload.put_u8(0); // seq
        payload.put_u16(0, order); // change
        payload.extend_from_slice(&[0u8; 16]); // addr wildcard → peer
        payload.put_u16(5075, order); // port
        encode_string_into("tcp", order, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
        let mut base = Vec::new();
        header.write_into(&mut base);
        base.extend_from_slice(&payload);

        // Segmented (any segment bit): dropped — no event, not tracked.
        for seg in [
            HeaderFlags::SEGMENT_FIRST,
            HeaderFlags::SEGMENT_LAST,
            HeaderFlags::SEGMENT_MASK,
        ] {
            let mut frame = base.clone();
            frame[2] |= seg;
            let beacons = BeaconTracker::new();
            let (tx, mut rx) = mpsc::channel::<Discovered>(8);
            let mut subs: Vec<mpsc::Sender<Discovered>> = vec![tx];
            let mut poke = false;
            let consumed = handle_beacon(&frame, &beacons, &mut subs, &mut poke, peer);
            assert!(
                consumed > 0,
                "segmented beacon frame must still be advanced"
            );
            assert!(
                beacons.guid_for(server).is_none(),
                "segmented UDP BEACON (bits {seg:#04x}) must not enter the tracker"
            );
            assert!(
                rx.try_recv().is_err(),
                "segmented UDP BEACON (bits {seg:#04x}) must emit no Discovered event"
            );
        }

        // Control: the un-segmented beacon tracks + emits Online.
        let beacons = BeaconTracker::new();
        let (tx, mut rx) = mpsc::channel::<Discovered>(8);
        let mut subs: Vec<mpsc::Sender<Discovered>> = vec![tx];
        let mut poke = false;
        handle_beacon(&base, &beacons, &mut subs, &mut poke, peer);
        assert_eq!(
            beacons.guid_for(server),
            Some(G),
            "un-segmented BEACON must enter the tracker"
        );
        assert!(
            matches!(rx.try_recv(), Ok(Discovered::Online { guid, .. }) if guid == G),
            "un-segmented BEACON must emit Discovered::Online"
        );
    }

    /// A live discovery subscriber whose bounded queue is full must NOT be
    /// evicted: pvxs keeps the operation until the caller cancels it
    /// (clientdiscover.cpp:103-112) and never treats a slow consumer as
    /// cancellation. The subscriber is removed only when its receiver is
    /// dropped (`Closed`). Regression for treating `Full` as removal.
    #[test]
    fn full_queue_keeps_live_subscriber_closed_removes_it() {
        let sa = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5075);
        let mk = |n: u8| Discovered::Online {
            server: sa,
            guid: [n; 12],
            peer: sa,
            proto: "tcp".into(),
        };
        let (tx, mut rx) = mpsc::channel::<Discovered>(2);
        let mut subs: Vec<mpsc::Sender<Discovered>> = vec![tx];

        // Fill the 2-slot queue.
        publish_discovery(&mut subs, mk(1));
        publish_discovery(&mut subs, mk(2));
        assert_eq!(subs.len(), 1, "subscriber present after filling the queue");

        // Queue full → event is dropped but the subscriber is RETAINED.
        publish_discovery(&mut subs, mk(3));
        assert_eq!(
            subs.len(),
            1,
            "a full queue must not unsubscribe a live consumer"
        );

        // Drain one slot, then a later event is delivered to the same sub.
        assert!(rx.try_recv().is_ok());
        publish_discovery(&mut subs, mk(4));
        assert_eq!(subs.len(), 1, "subscriber still live after draining");

        // Dropping the receiver closes the channel → subscriber removed.
        drop(rx);
        publish_discovery(&mut subs, mk(5));
        assert!(
            subs.is_empty(),
            "a closed receiver must unsubscribe the discovery operation"
        );
    }

    /// Regression R0604-PVACLI-DISCOVERY-TIMEOUT-PROTO-1.
    /// A GUID/peerVersion change on the `tls` identity must emit
    /// `Timeout{proto:"tls", ..}` carrying the OLD GUID + peerVersion, then
    /// `Online{proto:"tls", ..}` for the new incarnation — mirroring pvxs
    /// `onBeacon` (client.cpp:814-844). Before the fix the Timeout carried
    /// no proto, so a consumer could not tell which protocol's incarnation
    /// expired and a `tls` change was indistinguishable from a `tcp` one.
    #[test]
    fn changed_emits_proto_scoped_timeout_then_online() {
        let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 5075);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 9)), 5076);
        let (tx, mut rx) = mpsc::channel::<Discovered>(8);
        let mut subs: Vec<mpsc::Sender<Discovered>> = vec![tx];

        let poke = emit_beacon_action(
            BeaconAction::Changed {
                old_guid: [1u8; 12],
                old_peer_version: 2,
            },
            server,
            [2u8; 12], // new GUID
            peer,
            "tls".to_string(),
            &mut subs,
        );
        assert!(poke, "a Change must poke pending searches");

        // First event: Timeout for the OLD tls incarnation.
        match rx.try_recv() {
            Ok(Discovered::Timeout {
                server: s,
                guid,
                proto,
                peer_version,
            }) => {
                assert_eq!(s, server);
                assert_eq!(guid, [1u8; 12], "Timeout carries the OLD guid");
                assert_eq!(proto, "tls", "Timeout is scoped to the tls identity");
                assert_eq!(peer_version, 2, "Timeout carries the OLD peerVersion");
            }
            other => panic!("expected Timeout{{proto:\"tls\"}} first, got {other:?}"),
        }

        // Second event: Online for the NEW tls incarnation, same proto.
        match rx.try_recv() {
            Ok(Discovered::Online { guid, proto, .. }) => {
                assert_eq!(guid, [2u8; 12], "Online carries the NEW guid");
                assert_eq!(proto, "tls");
            }
            other => panic!("expected Online{{proto:\"tls\"}} second, got {other:?}"),
        }

        assert!(rx.try_recv().is_err(), "exactly two events emitted");
    }

    /// PVA-RS-2026-05-28-114: a found=true SEARCH_RESPONSE is resolved
    /// only when its transport protocol is exactly `"tcp"`. pvxs drops
    /// any other found reply — including an empty/null-marker protocol —
    /// and keeps searching (client.cpp:872-904). An empty protocol used
    /// to be tolerated as a back-compat exception, which let a malformed
    /// or legacy responder satisfy a search and make the client dial a
    /// plain TCP circuit to an endpoint that never advertised `"tcp"`.
    #[test]
    fn found_true_requires_exact_tcp_protocol() {
        use tokio::sync::oneshot;

        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);

        let make_pending = || -> (HashMap<u32, Pending>, HashMap<String, u32>, oneshot::Receiver<SearchHit>) {
            let (tx, rx) = oneshot::channel::<SearchHit>();
            let mut pending: HashMap<u32, Pending> = HashMap::new();
            pending.insert(
                1,
                Pending {
                    pv_name: "dut".into(),
                    responder: Responder::Single(tx),
                    last_attempt: Instant::now(),
                    attempt: 1,
                    bucket: 0,
                },
            );
            let mut by_name: HashMap<String, u32> = HashMap::new();
            by_name.insert("dut".into(), 1);
            (pending, by_name, rx)
        };

        let found_true = |proto: &str| -> Vec<u8> {
            crate::server_native::udp::build_search_response_proto(
                [0x42u8; 12],
                0,
                5075,
                &[1], // non-empty cids ⇒ found byte encodes true
                ByteOrder::Little,
                proto,
            )
        };

        // `""` here also stands in for the null-string-marker case: the
        // decoder maps a null marker to an empty protocol, which must
        // fail the exact `== "tcp"` gate just like a literal empty field.
        for proto in ["", "tls", "TCP", "udp"] {
            let (mut pending, mut by_name, _rx) = make_pending();
            let beacons = BeaconTracker::new();
            let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
            let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
            let mut poke = false;
            handle_search_response(
                &found_true(proto),
                &mut pending,
                &mut by_name,
                &beacons,
                &ignore,
                &mut subs,
                &mut poke,
                peer,
                false,
            );
            assert!(
                pending.contains_key(&1),
                "found=true with protocol {proto:?} must be ignored, not resolved"
            );
        }

        // Exactly "tcp" resolves the pending channel.
        let (mut pending, mut by_name, mut rx) = make_pending();
        let beacons = BeaconTracker::new();
        let ignore: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        let mut subs: Vec<mpsc::Sender<Discovered>> = Vec::new();
        let mut poke = false;
        handle_search_response(
            &found_true("tcp"),
            &mut pending,
            &mut by_name,
            &beacons,
            &ignore,
            &mut subs,
            &mut poke,
            peer,
            false,
        );
        assert!(
            !pending.contains_key(&1),
            "found=true with protocol \"tcp\" must resolve the pending channel"
        );
        assert!(
            rx.try_recv().is_ok(),
            "the resolver must receive the server address"
        );
    }

    /// A BEACON whose protocol string is *truncated* (claimed length runs
    /// past the datagram) is malformed and must be dropped — it must NOT
    /// enter the beacon tracker, announce the server, or emit
    /// `Discovered::Online`. pvxs faults the decode buffer only on that
    /// truncation (`!buf.ensure(len)`, pvaproto.h:399-400) and invokes the
    /// beacon callbacks only while the buffer is good
    /// (udp_collector.cpp:478-488). The pre-fix code coerced any decode
    /// failure to a fabricated `"tcp"` protocol and proceeded.
    ///
    /// An *invalid-UTF8 but length-complete* protocol string is, by
    /// contrast, well-formed on the wire: pvxs copies the raw bytes with no
    /// UTF-8 validation (pvaproto.h:403), the buffer stays good, and the
    /// beacon announces (the tracker keys on (server, proto) with no
    /// `proto=="tcp"` filter, client.cpp:780). PVA-89 made `decode_string`
    /// lossy to match, so this case must announce — not drop.
    #[test]
    fn malformed_beacon_protocol_is_dropped() {
        use crate::proto::{WriteExt, encode_size_into, encode_string_into};

        let order = ByteOrder::Little;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);

        // proto_writer appends the protocol-string region of the payload.
        let build_beacon = |proto_writer: &dyn Fn(&mut Vec<u8>)| -> Vec<u8> {
            let mut payload = Vec::new();
            payload.extend_from_slice(&[0x42u8; 12]); // guid
            payload.put_u8(0); // flags
            payload.put_u8(0); // seq
            payload.put_u16(0, order); // change
            payload.extend_from_slice(&[0u8; 16]); // addr (wildcard → peer)
            payload.put_u16(5075, order); // port
            proto_writer(&mut payload);
            let header =
                PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
            let mut frame = Vec::new();
            header.write_into(&mut frame);
            frame.extend_from_slice(&payload);
            frame
        };

        // Returns (Discovered::Online fired, server entered the tracker).
        let run = |frame: &[u8]| -> (bool, bool) {
            let beacons = BeaconTracker::new();
            let (tx, mut rx) = mpsc::channel::<Discovered>(8);
            let mut subs: Vec<mpsc::Sender<Discovered>> = vec![tx];
            let mut poke = false;
            handle_beacon(frame, &beacons, &mut subs, &mut poke, peer);
            let server = SocketAddr::new(peer.ip(), 5075);
            (rx.try_recv().is_ok(), beacons.guid_for(server).is_some())
        };

        // Valid "tcp" beacon → tracked + Discovered::Online fires.
        let valid = build_beacon(&|p| encode_string_into("tcp", order, p));
        let (online, tracked) = run(&valid);
        assert!(online, "valid BEACON must emit Discovered::Online");
        assert!(tracked, "valid BEACON must enter the tracker once");

        // Truncated protocol: size claims 5 bytes, none follow.
        let truncated = build_beacon(&|p| encode_size_into(5, order, p));
        let (online, tracked) = run(&truncated);
        assert!(!online, "truncated-protocol BEACON must not announce");
        assert!(!tracked, "truncated-protocol BEACON must be dropped");

        // Invalid UTF-8 protocol payload (0xC3 0x28 is not valid UTF-8) but
        // length-complete: pvxs copies the raw bytes (pvaproto.h:403), the
        // buffer stays good, and onBeacon announces with no "tcp" filter
        // (client.cpp:780). PVA-89's lossy `decode_string` matches — the
        // beacon must announce and enter the tracker, NOT be dropped.
        let bad_utf8 = build_beacon(&|p| {
            encode_size_into(2, order, p);
            p.extend_from_slice(&[0xC3, 0x28]);
        });
        let (online, tracked) = run(&bad_utf8);
        assert!(
            online,
            "length-complete invalid-UTF8-protocol BEACON must announce (pvxs parity)"
        );
        assert!(
            tracked,
            "length-complete invalid-UTF8-protocol BEACON must enter the tracker"
        );
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

    /// pvxs poke preserves per-channel `nSearch` (client.cpp:1141-1160): a poked
    /// tick must NOT escalate the backoff. An aged search at `attempt = 7`,
    /// after a HurryUp/beacon poke, must requeue into the SAME bucket with its
    /// attempt unchanged — not restart as a fresh 1-bucket retry at attempt 1.
    #[test]
    fn rearm_poked_preserves_backoff_and_requeues_same_bucket() {
        let no_imbalance = |_| 0usize;
        let current = 5;

        // Poked tick: attempt preserved, requeued into the same bucket
        // regardless of how large the accumulated backoff is.
        assert_eq!(
            rearm_after_send(true, current, 7, no_imbalance),
            (current, 7),
            "a poked tick keeps nSearch and requeues into the same bucket"
        );
        assert_eq!(
            rearm_after_send(true, current, 1, no_imbalance),
            (current, 1),
            "poked requeue is independent of attempt value"
        );

        // Normal tick: nSearch increments first, then escalates forward by the
        // incremented count.
        assert_eq!(
            rearm_after_send(false, current, 7, no_imbalance),
            ((current + 8) % N_SEARCH_BUCKETS, 8),
            "a normal tick increments nSearch and escalates the bucket"
        );
        assert_eq!(
            rearm_after_send(false, current, 0, no_imbalance),
            ((current + 1) % N_SEARCH_BUCKETS, 1),
            "first normal retry escalates one bucket forward"
        );
    }

    /// pvxs `poke()` (client.cpp:736-759) and the discovery-pong path
    /// (client.cpp:889-899) only arm the fast search revolution — they
    /// never iterate channels or reset `nSearch`. The revolution's poked
    /// ticks then preserve each search's accumulated backoff
    /// (`tickSearch(..., poked=true)` skips the `nSearch++` increment,
    /// client.cpp:1141-1143). A real discovery pong must therefore leave a
    /// pending search's `attempt`/`bucket` untouched and only signal the
    /// poke. Before, the UDP discovery-pong handler reset every pending
    /// search to `attempt = 0` / `last_attempt = now-60s`, converting an
    /// escalated backoff into a fresh 1-bucket retry on each beacon — a
    /// search-broadcast amplifier on a mass IOC restart.
    #[test]
    fn discovery_pong_preserves_pending_search_backoff() {
        use tokio::sync::oneshot;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 5076);
        let (tx, _rx) = oneshot::channel::<SearchHit>();
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        pending.insert(
            1,
            Pending {
                pv_name: "dut".into(),
                responder: Responder::Single(tx),
                last_attempt: Instant::now(),
                attempt: 3,
                bucket: 2,
            },
        );
        let mut by_name: HashMap<String, u32> = std::iter::once(("dut".to_string(), 1)).collect();
        let beacons = BeaconTracker::new();
        // An active discover() subscriber — required for the discovery-pong
        // branch to fire at all (pvxs `!discoverers.empty()`,
        // client.cpp:889). Keep `_rxd` alive so the channel stays open.
        let (txd, _rxd) = mpsc::channel::<Discovered>(8);
        let mut subs: Vec<mpsc::Sender<Discovered>> = vec![txd];
        let mut poke = false;
        // found=false (empty cids), seq == SEARCH_SEQ, fresh GUID → a New
        // beacon identity → should_poke.
        let pong = crate::server_native::udp::build_search_response_proto(
            [0x99u8; 12],
            SEARCH_SEQ,
            5075,
            &[],
            ByteOrder::Little,
            "tcp",
        );
        handle_search_response(
            &pong,
            &mut pending,
            &mut by_name,
            &beacons,
            &std::collections::HashSet::new(),
            &mut subs,
            &mut poke,
            peer,
            false,
        );
        assert!(poke, "a New discovery-pong identity must signal a poke");
        let p = pending.get(&1).expect("pending search must survive a poke");
        assert_eq!(
            p.attempt, 3,
            "discovery pong must NOT reset the pending search's accumulated backoff"
        );
        assert_eq!(
            p.bucket, 2,
            "discovery pong must NOT move the pending search to a fresh bucket"
        );
    }

    /// `find_all` must NOT hang indefinitely when no server claims the
    /// PV — review finding #3. With the fix, the tick handler flushes
    /// the Multi responder at deadline even with empty `accumulated`,
    /// so the user-visible future resolves to `Vec::new()` and any
    /// outer `PvaClient::timeout` gets a chance to apply.
    #[tokio::test(flavor = "current_thread")]
    #[serial(epics_env)]
    async fn find_all_returns_empty_when_no_responder() {
        // Suppress UDP fan-out — we don't want the engine bound to
        // 5076 in CI / racing with a real PVA server. The guard
        // restores the prior values on drop.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

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
    #[serial(epics_env)]
    async fn reconnect_search_broadcasts_within_one_tick() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        // Suppress real broadcast targets so the only destination
        // is our sniffer below.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

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
    #[serial(epics_env)]
    async fn reconnect_find_does_not_complete_without_response() {
        // Suppress real broadcast so no actual SEARCH leaves the
        // process to potentially get answered by some other PVA
        // server on the LAN.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);
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
    #[serial(epics_env)]
    async fn hurry_up_kicks_pending_searches_at_fast_tick_cadence() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);
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
    #[serial(epics_env)]
    async fn retry_escalation_pvxs_pattern() {
        use epics_base_rs::net::AsyncUdpV4;
        use std::net::Ipv4Addr;

        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);
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
    #[serial(epics_env)]
    async fn client_search_resolves_over_ipv6() {
        use crate::nt::typed::TypedNT;
        use crate::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};
        use std::net::Ipv6Addr;

        // Suppress NIC broadcast so accidental v4 traffic to a sibling
        // pva-rs server on this host can't resolve the PV name.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

        let pv = SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&2.5)).unwrap();
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
            resolved.server.port(),
            server_tcp_port,
            "resolved TCP port must match v6 server's listener"
        );
        assert!(
            matches!(resolved.server.ip(), IpAddr::V6(_)),
            "resolved server address must be IPv6; got {resolved:?}"
        );
        drop(server);
    }

    /// Regression R0604-PVASRV-V6-WILDCARD-SEARCH-PORT-1 (client half):
    /// the v6 SEARCH frame must advertise the v6 search socket's own port
    /// — the port it actually receives SEARCH_RESPONSE on — not the v4
    /// search socket's port. A pvxs-compatible server honours the
    /// advertised `response_port` (`udp_collector.cpp:380 setPort`), so
    /// advertising the wrong (v4) port routes the reply to a port nothing
    /// listens on over IPv6.
    ///
    /// FAIL-proof: capture the discover SEARCH the engine emits to a v6
    /// target. The datagram's source port is the engine's live v6 socket
    /// port, and the advertised `response_port` inside the frame must
    /// equal it. Before the fix the v6 frame carried the v4 socket port
    /// (≠ source) and this assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(epics_env)]
    async fn v6_search_advertises_v6_socket_port() {
        use std::net::{Ipv6Addr, SocketAddrV6};

        // No auto broadcast / no addr-list: the engine sends only to our
        // explicit v6 sniffer target.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

        // v6 sniffer the engine will broadcast its discover SEARCH to.
        let sniffer = tokio::net::UdpSocket::bind("[::1]:0")
            .await
            .expect("bind v6 sniffer");
        let sniffer_port = sniffer.local_addr().expect("sniffer addr").port();
        let v6_target = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, sniffer_port, 0, 0));

        let engine = SearchEngine::spawn(vec![v6_target], Vec::new())
            .await
            .expect("spawn engine");

        // DiscoverPing fires one discover SEARCH immediately to every
        // target (no initial-search coalescing window to wait out).
        engine.ping_all().await;

        let mut buf = vec![0u8; 1024];
        let (n, src) = tokio::time::timeout(Duration::from_secs(5), sniffer.recv_from(&mut buf))
            .await
            .expect("v6 discover SEARCH did not arrive (is IPv6 loopback available?)")
            .expect("recv_from");

        // The datagram's source port IS the engine's live v6 search socket
        // port — the port it will receive SEARCH_RESPONSE on.
        let v6_socket_port = match src {
            SocketAddr::V6(v6) => v6.port(),
            other => panic!("v6 sniffer received non-v6 source {other}"),
        };

        // Decode the advertised `response_port`. `run_engine` builds
        // little-endian frames: 8-byte PVA header + payload {seq(4) +
        // flags(1) + reserved(3) + reply addr(16) + response_port(2) + …},
        // so the port sits at frame offset 8+4+1+3+16 = 32.
        assert!(n >= 34, "discover SEARCH too short to hold response_port");
        let advertised = u16::from_le_bytes([buf[32], buf[33]]);

        assert_eq!(
            advertised, v6_socket_port,
            "v6 SEARCH must advertise its own v6 socket port ({v6_socket_port}), \
             not a different (v4) port ({advertised})"
        );

        drop(engine);
    }

    /// PR #205 IPv6 Stage 6: `bind_beacon_udp_v6` returns a usable
    /// socket bound to `[::]:5076` (or `EPICS_PVA_BROADCAST_PORT`)
    /// with the default v6 multicast group joined. Confirms the
    /// plumbing — without it the recv arm in `run_engine` never
    /// fires for v6 beacons.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(epics_env)]
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
        let port_str = port.to_string();
        let _env = EnvVarGuard::set(&[("EPICS_PVA_BROADCAST_PORT", &port_str)]);

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

        drop(sock);
    }

    /// PR #205 IPv6 Stage 6 end-to-end: spawn a SearchEngine and emit
    /// a synthetic v6 beacon to its v6 beacon socket. The engine's
    /// recv arm decodes the beacon, the BeaconTracker observes the
    /// (server_addr, guid) pair, and `beacon_guid_for(addr)` returns
    /// the GUID we sent. Guards the full recv-decode-track chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial(epics_env)]
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
        let port_str = port.to_string();
        // Suppress v4 search destinations so the engine's broadcast
        // loop has nothing useful to fire.
        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_BROADCAST_PORT", &port_str),
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

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

        let observed = found_guid.expect(
            "BeaconTracker must observe a beacon arriving on the v6 beacon socket; \
             v6 recv arm in run_engine may be broken",
        );
        assert_eq!(observed, guid, "tracker must record the exact GUID");
    }

    /// Regression: TCP name servers must resolve PVs via persistent
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
    #[serial(epics_env)]
    async fn pva_r4_tcp_nameserver_persistent_peer() {
        use std::io::Cursor as IoCursor;
        use tokio::net::TcpListener;

        // Bind mock NS before spawning engine so the port is known.
        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock NS listener bind");
        let ns_addr = ns_listener.local_addr().unwrap();

        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

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
                payload.put_u32(0x10000, order); // buffer_size (match pvxs 0x10000)
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

        ns_handle.abort();

        let resolved = result
            .expect("find() must complete within 5 s; TCP NS search path may be broken")
            .expect("find() must succeed; handle_search_response may not route TCP responses");
        assert_eq!(
            resolved.server.port(),
            ns_addr.port(),
            "resolved port must match what the NS advertised in SEARCH_RESPONSE"
        );
        assert_eq!(
            resolved.server.ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            "resolved IP must be 127.0.0.1 (rewrite_loopback from unspecified)"
        );
    }

    /// A TCP name server that advertises `anonymous, ca` must see the client
    /// select `ca` and send the configured user/host — pvxs negotiates CA on
    /// name-server connections exactly as on normal servers (clientconn.cpp:215-263),
    /// rather than forcing anonymous. Captures the client's CONNECTION_VALIDATION
    /// reply on the mock NS and asserts the negotiated method + credentials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial(epics_env)]
    async fn pva_rs_51_tcp_nameserver_selects_ca_with_credentials() {
        use std::io::Cursor as IoCursor;
        use tokio::net::TcpListener;

        let ns_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock NS listener bind");
        let ns_addr = ns_listener.local_addr().unwrap();

        let _env = EnvVarGuard::set(&[
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_PVA_ADDR_LIST", ""),
        ]);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

        let ns_handle = tokio::spawn(async move {
            let (mut stream, _peer) = ns_listener.accept().await.expect("mock NS: accept");
            let order = ByteOrder::Little;

            // SET_BYTE_ORDER
            let mut sbo = Vec::with_capacity(PvaHeader::SIZE);
            PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0)
                .write_into(&mut sbo);
            stream.write_all(&sbo).await.expect("mock NS: write SBO");

            // CONNECTION_VALIDATION request advertising both methods.
            {
                let mut payload = Vec::new();
                payload.put_u32(0x10000, order);
                payload.put_u16(32_767, order);
                payload.push(2u8); // auth_methods count (size-encoded, < 254)
                encode_string_into("anonymous", order, &mut payload);
                encode_string_into("ca", order, &mut payload);
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

            // Capture the client's CONNECTION_VALIDATION reply payload.
            let mut buf = Vec::<u8>::new();
            let mut tmp = [0u8; 4096];
            let reply_payload = 'capture: loop {
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
                        break 'capture buf[pos + PvaHeader::SIZE..frame_end].to_vec();
                    }
                    pos = frame_end;
                }
            };
            let _ = reply_tx.send(reply_payload);
        });

        let _engine = SearchEngine::spawn_with_auth(
            Vec::new(),
            vec![ns_addr],
            "operator".to_string(),
            "myhost".to_string(),
            Duration::from_secs(5),
        )
        .await
        .expect("spawn engine");

        let payload = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("client must send CONNECTION_VALIDATION reply within 5 s")
            .expect("mock NS captured the reply");
        ns_handle.abort();

        // Reply layout: buffer_size(4) + registry_size(2) + qos(2) + auth(string).
        let auth = decode_string(&mut Cursor::new(&payload[8..]), ByteOrder::Little)
            .expect("decode negotiated auth method")
            .expect("auth method string present");
        assert_eq!(
            auth, "ca",
            "client must select ca when the NS advertises anonymous, ca"
        );
        // The CA variant carries the user/host as string values; assert the
        // configured credentials appear (distinct from the b\"user\"/b\"host\"
        // field-name descriptors).
        assert!(
            payload.windows(b"operator".len()).any(|w| w == b"operator"),
            "CA reply must carry the configured user"
        );
        assert!(
            payload.windows(b"myhost".len()).any(|w| w == b"myhost"),
            "CA reply must carry the configured host"
        );
    }

    // ---- search_targets: AUTO_ADDR_LIST gating (pvxs expandAddrList) ----

    /// AUTO_ADDR_LIST=NO with an empty configured address list must yield
    /// NO destinations — pvxs skips expandAddrList and sends only to the
    /// (empty) addressList, so no SEARCH is emitted at all. The pre-fix
    /// code unconditionally pushed 255.255.255.255, leaking broadcast onto
    /// a LAN the operator intentionally restricted.
    #[test]
    fn search_targets_empty_when_auto_off_and_no_extras() {
        let targets = search_targets(5076, false, &[], &[]);
        assert!(
            targets.is_empty(),
            "AUTO_ADDR_LIST=NO + empty list must emit no broadcast; got {targets:?}"
        );
    }

    /// Regression R0604-PVA-CLIENT-INTF-EXPLICIT-ADDR-1.
    ///
    /// `EPICS_PVA_INTF_ADDR_LIST` must NOT reduce the active-search socket
    /// bundle. pvxs binds the search socket to wildcard
    /// (`client.cpp:578-590`) and applies the interface list only to
    /// auto-broadcast generation + beacon receive; explicit unicast
    /// `EPICS_PVA_ADDR_LIST` targets route via the OS from the full bundle.
    /// Before the fix, a loopback-only interface list bound a loopback-only
    /// bundle, so `pick_nic` forced an explicit non-loopback target onto the
    /// loopback socket (last-resort) where it could never reach the IOC.
    #[tokio::test]
    async fn search_socket_bundle_not_reduced_by_intf_addr_list() {
        // The defect can only manifest where the host actually has a
        // non-loopback IPv4 NIC; on a loopback-only host the bundle is
        // loopback-only on either code path, so the boundary is absent.
        let host_has_non_loopback = if_addrs::get_if_addrs()
            .map(|v| {
                v.iter()
                    .any(|i| matches!(i.addr, if_addrs::IfAddr::V4(_)) && !i.is_loopback())
            })
            .unwrap_or(false);
        if !host_has_non_loopback {
            return;
        }
        // The interface list is irrelevant to the bundle now — bind the
        // search socket the way `spawn_inner` does and require a
        // non-loopback bind addr to be present.
        let sock = bind_ephemeral_udp().expect("search socket bind");
        let addrs = sock.local_addrs();
        assert!(
            addrs.iter().any(|a| !a.ip().is_loopback()),
            "active-search socket bundle was reduced to loopback only; an \
             explicit non-loopback EPICS_PVA_ADDR_LIST target would be forced \
             onto the loopback socket and never reach the IOC. Bound: {addrs:?}"
        );
    }

    /// EPICS_PVA_INTF_ADDR_LIST=127.0.0.1 with AUTO_ADDR_LIST=YES must
    /// produce no non-loopback broadcast target — the interface list
    /// constrains auto address expansion (pvxs `config.cpp:624-648`).
    #[test]
    fn search_targets_loopback_only_interface_list_no_broadcast() {
        let targets = search_targets(5076, true, &[], &[Ipv4Addr::LOCALHOST]);
        assert!(
            !targets.iter().any(|t| match t {
                SocketAddr::V4(v4) => !v4.ip().is_loopback(),
                SocketAddr::V6(_) => true,
            }),
            "loopback-only interface list produced a non-loopback target: {targets:?}"
        );
        assert!(
            !targets
                .iter()
                .any(|t| matches!(t, SocketAddr::V4(v4) if v4.ip().is_broadcast())),
            "loopback-only interface list must not emit limited broadcast: {targets:?}"
        );
        // The loopback unicast convenience is still present (loopback is
        // the explicitly-listed interface).
        assert!(
            targets.contains(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5076)),
            "loopback unicast expected when loopback is the listed interface"
        );
    }

    /// AUTO_ADDR_LIST=NO sends only to the explicitly configured targets,
    /// never the limited broadcast.
    #[test]
    fn search_targets_auto_off_sends_only_configured_extras() {
        let extra: SocketAddr = "10.0.0.5:5076".parse().unwrap();
        let targets = search_targets(5076, false, &[extra], &[]);
        assert_eq!(targets, vec![extra]);
        assert!(
            !targets
                .iter()
                .any(|t| matches!(t, SocketAddr::V4(v4) if v4.ip().is_broadcast())),
            "no limited broadcast may be added when AUTO_ADDR_LIST is off"
        );
    }

    /// AUTO_ADDR_LIST=YES (the default) still includes the limited
    /// broadcast destination, and the configured extras are appended.
    #[test]
    fn search_targets_auto_on_includes_limited_broadcast_and_extras() {
        let extra: SocketAddr = "10.0.0.5:5076".parse().unwrap();
        let targets = search_targets(5076, true, &[extra], &[]);
        assert!(
            targets.contains(&SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 5076)),
            "AUTO_ADDR_LIST=YES must add the limited broadcast destination"
        );
        assert!(
            targets.contains(&extra),
            "configured extras are always included"
        );
    }

    /// PVA parity: the default auto-address path (AUTO_ADDR_LIST=YES, no
    /// EPICS_PVA_INTF_ADDR_LIST) must NOT inject an implicit 127.0.0.1
    /// unicast target. pvxs's expandAddrList returns only discovered
    /// broadcast addresses (`config.cpp:624-648` → `evhelper.cpp:625-660`);
    /// loopback-only discovery requires an explicit address-list entry,
    /// which arrives via `extra_targets`, not the auto path.
    #[test]
    fn search_targets_default_auto_path_has_no_implicit_loopback() {
        let lo = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5076);
        let targets = search_targets(5076, true, &[], &[]);
        assert!(
            !targets.contains(&lo),
            "default auto-address path must not inject an implicit loopback target: {targets:?}"
        );
        // An explicit EPICS_PVA_ADDR_LIST=127.0.0.1 entry (carried in
        // extra_targets) still reaches loopback — the parity-correct path.
        let with_explicit = search_targets(5076, true, &[lo], &[]);
        assert!(
            with_explicit.contains(&lo),
            "explicit addr-list loopback entry must still target loopback: {with_explicit:?}"
        );
    }

    // ── TCP name-server readiness gate (no replayed backlog) ────────────────

    /// One-name helper mirroring the production single-PV NS forward:
    /// pack the name into the name-server frame shape (port 0, unicast)
    /// and forward to ready connections.
    fn ns_forward_one(handles: &[NsHandle], codec: &PvaCodec, sid: u32, pv_name: &str) {
        ns_forward_frames(
            handles,
            &pack_search_frames(codec, &[(sid, pv_name.to_string())], 0, true),
        );
    }

    #[test]
    fn ns_search_to_ready_skips_not_ready_and_keeps_no_backlog() {
        let codec = PvaCodec { big_endian: false };
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let ready = Arc::new(AtomicBool::new(false));
        let handles = vec![NsHandle {
            tx,
            ready: Arc::clone(&ready),
        }];

        // NS offline/handshaking: ticks far exceeding the channel capacity must
        // not buffer a single frame, so there is nothing to burst on reconnect.
        for sid in 0..200u32 {
            ns_forward_one(&handles, &codec, sid, "PV:NS");
        }
        assert!(
            rx.try_recv().is_err(),
            "no SEARCH may be queued while the NS is not ready"
        );

        // NS validated: only the current tick is written, once.
        ready.store(true, Ordering::SeqCst);
        ns_forward_one(&handles, &codec, 42, "PV:NS");
        assert!(
            rx.try_recv().is_ok(),
            "the current-tick SEARCH is delivered once the NS is ready"
        );
        assert!(
            rx.try_recv().is_err(),
            "exactly one current-tick frame, no replayed stale backlog"
        );
    }

    #[test]
    fn ns_search_to_ready_targets_only_ready_connections() {
        let codec = PvaCodec { big_endian: false };
        let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(64);
        let (tx_b, mut rx_b) = mpsc::channel::<Vec<u8>>(64);
        let handles = vec![
            NsHandle {
                tx: tx_a,
                ready: Arc::new(AtomicBool::new(true)),
            },
            NsHandle {
                tx: tx_b,
                ready: Arc::new(AtomicBool::new(false)),
            },
        ];
        ns_forward_one(&handles, &codec, 7, "PV:X");
        assert!(rx_a.try_recv().is_ok(), "ready NS receives the SEARCH");
        assert!(
            rx_b.try_recv().is_err(),
            "not-ready NS is skipped this tick"
        );
    }

    /// PVA-RS-2026-05-28-63: many channel names pack into as few
    /// datagrams as fit under MAX_SEARCH_PAYLOAD, each carrying the
    /// channel count once — not one datagram per name.
    #[test]
    fn pack_search_frames_batches_under_payload_limit() {
        let codec = PvaCodec { big_endian: false };

        // A handful of short names fit in a single datagram.
        let entries: Vec<(u32, String)> = (0..8u32).map(|i| (i, format!("PV:SHORT:{i}"))).collect();
        let frames = pack_search_frames(&codec, &entries, 5076, false);
        assert_eq!(frames.len(), 1, "8 short names must pack into one datagram");
        assert!(frames[0].len() <= MAX_SEARCH_PAYLOAD);
        // Channel count is written once: the u16 right after the 8-byte
        // frame header + 4 (seq) + 1 (flags) + 3 (reserved) + 16 (addr)
        // + 2 (port) + 1 (proto size) + 4 ("tcp") = offset 39.
        let count = u16::from_le_bytes([frames[0][39], frames[0][40]]);
        assert_eq!(count, 8, "all 8 names share one channel-count field");

        // Many names spill into multiple datagrams, each under the limit.
        let many: Vec<(u32, String)> = (0..400u32)
            .map(|i| (i, format!("PV:LONGISH:NAME:NUMBER:{i:05}")))
            .collect();
        let frames = pack_search_frames(&codec, &many, 5076, false);
        assert!(
            frames.len() > 1,
            "400 names must spill across multiple datagrams, got {}",
            frames.len()
        );
        for f in &frames {
            assert!(
                f.len() <= MAX_SEARCH_PAYLOAD,
                "each packed datagram stays under the MTU guard"
            );
        }
        // Empty input produces no frames.
        assert!(pack_search_frames(&codec, &[], 5076, false).is_empty());
    }

    /// Every regular SEARCH carries the fixed pvxs `search_seq` ("find",
    /// 0x6669_6e64) in its searchSequenceID, not 0. pvxs stamps this single
    /// value on EVERY search via the shared `tickSearch` loop
    /// (client.cpp:1072); the field is redundant but the contract is a
    /// fixed value, not zero.
    #[test]
    fn pack_search_frames_stamps_search_seq() {
        let codec = PvaCodec { big_endian: false };
        let entries = vec![(7u32, "PV:SEQ".to_string())];
        let frames = pack_search_frames(&codec, &entries, 5076, false);
        assert_eq!(frames.len(), 1);
        // The PVA payload begins after the 8-byte frame header; its first
        // u32 is the searchSequenceID.
        let seq = u32::from_le_bytes([frames[0][8], frames[0][9], frames[0][10], frames[0][11]]);
        assert_eq!(
            seq, SEARCH_SEQ,
            "regular SEARCH must stamp search_seq, not 0"
        );
        assert_eq!(
            SEARCH_SEQ, 0x6669_6e64,
            "search_seq is the ASCII bytes \"find\""
        );
    }
}
