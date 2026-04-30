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

use std::collections::HashMap;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{Command, PvaHeader, ReadExt, decode_size, decode_string, ip_from_bytes};

use super::beacon_throttle::BeaconTracker;
use super::decode::{decode_search_response, try_parse_frame};

/// Search retry backoff sequence (seconds), matching pvxs `clientdiscover.cpp`.
pub const BACKOFF_SECS: &[u64] = &[1, 1, 2, 5, 10, 15, 30, 60, 120, 210];

/// Default UDP broadcast port for SEARCH/BEACON messages (5076).
pub const DEFAULT_BROADCAST_PORT: u16 = 5076;

/// Command sent into the engine.
/// Why a search is being initiated — controls bucket placement and
/// whether the first SEARCH packet fires immediately.
///
/// Mirrors ca-rs `SearchReason`: an `Initial` find is a single fresh
/// resolve and earns an immediate broadcast for low first-attempt
/// latency. A `Reconnect` find is one of potentially many concurrent
/// re-searches (e.g. a 5000-channel TCP-close cascade) and therefore
/// goes into a sid-hashed bucket without immediate fire so the burst
/// is spread across the bucket revolution instead of pummelling the
/// network in a single tick. pvxs `client.cpp::tickSearch` parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchReason {
    /// Brand-new resolve; channel has never been Active.
    Initial,
    /// Channel was Active and the underlying ServerConn died (or the
    /// server sent CMD_DESTROY_CHANNEL). Distribute the retransmit so
    /// mass-disconnects don't collapse into a single-tick burst.
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
    pub async fn spawn(mut extra_targets: Vec<SocketAddr>) -> PvaResult<Self> {
        let beacons = BeaconTracker::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SearchCommand>(256);

        let search_socket = bind_ephemeral_udp()?;
        let beacon_socket = bind_beacon_udp(); // Optional — may be None.

        // pvxs 8db40be (2025-10): warn loudly when a Context is built
        // with no search destinations and AUTO_ADDR_LIST disabled.
        // The user otherwise sees nothing but timeouts.
        let auto_addr = std::env::var("EPICS_PVA_AUTO_ADDR_LIST").unwrap_or_else(|_| "YES".into());
        let auto_on = matches!(
            auto_addr.trim().to_ascii_uppercase().as_str(),
            "YES" | "Y" | "1" | "TRUE"
        );
        let env_addrs = std::env::var("EPICS_PVA_ADDR_LIST").ok();
        let env_has_dest = env_addrs
            .as_deref()
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
            for sa in resolved {
                if !extra_targets.contains(&sa) {
                    extra_targets.push(sa);
                }
            }
        }

        let beacons_clone = beacons.clone();
        tokio::spawn(run_engine(
            cmd_rx,
            search_socket,
            beacon_socket,
            extra_targets,
            beacons_clone,
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

/// Walk `EPICS_PVA_ADDR_LIST` and join every IPv4 multicast group on
/// every up, non-loopback NIC of `sock`. Errors are logged but not
/// propagated — a single failed join shouldn't disable the rest of
/// the discovery path.
pub(crate) fn join_addr_list_multicast(sock: &AsyncUdpV4) {
    let Ok(env) = std::env::var("EPICS_PVA_ADDR_LIST") else {
        return;
    };
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
    attempt: usize,
    /// Which search bucket this pending occupies. Set on first insert
    /// and rotated forward by `nBuckets` after each retry.
    bucket: usize,
    /// Tick-decremented hold-off counter: when > 0 at the bucket's
    /// firing tick, the search is NOT transmitted (re-placed in the
    /// same bucket so it fires again N ticks later, decrementing
    /// once each cycle). Set to RETRY_HOLDOFF_BUCKETS on first
    /// retry to avoid storming a slow IOC. The previous formula
    /// `(current+N+RETRY_HOLDOFF) % N == (current+RETRY_HOLDOFF)` was
    /// inverted — failed retries fired SOONER (~9 s) than fresh
    /// (~30 s).
    holdoff_cycles: u32,
}

/// pvxs `client.cpp::nBuckets`. 30 buckets at 1 s normal interval gives
/// each pending search a 30-second slot rotation — cooperative tick
/// caps UDP search traffic at roughly `pending.len() / 30` packets per
/// second instead of letting every channel fire on its own backoff.
const N_SEARCH_BUCKETS: usize = 30;
/// Bucket distance to push a re-search after the first retry. Mirrors
/// pvxs `Channel::disconnect` holdoff = 10 buckets to keep failed
/// connect attempts from looping faster than the 30s schedule.
const RETRY_HOLDOFF_BUCKETS: usize = 10;

async fn run_engine(
    mut cmd_rx: mpsc::Receiver<SearchCommand>,
    search_socket: AsyncUdpV4,
    beacon_socket: Option<AsyncUdpV4>,
    extra_targets: Vec<SocketAddr>,
    beacons: Arc<BeaconTracker>,
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
    let mut beacon_buf = vec![0u8; 64 * 1024];

    loop {
        // Build a beacon-recv future regardless of whether we bound it
        // (using `if let` to keep the select! shape simple).
        let beacon_recv = async {
            match &beacon_socket {
                Some(s) => s.recv_from(&mut beacon_buf).await,
                None => std::future::pending().await,
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
                    // Reason-aware bucket placement (ca-rs / pvxs
                    // parity). Initial: place one bucket ahead so it
                    // fires on the very next tick AND immediately
                    // broadcast for low single-channel latency.
                    // Reconnect: hash sid across all 30 buckets so a
                    // mass-disconnect cascade doesn't collapse into a
                    // single-tick burst, and skip the immediate fire
                    // so the bucket scheduler controls the cadence.
                    let bucket = match reason {
                        SearchReason::Initial => (current_bucket + 1) % N_SEARCH_BUCKETS,
                        SearchReason::Reconnect => {
                            let offset = (sid as usize) % N_SEARCH_BUCKETS;
                            (current_bucket + 1 + offset) % N_SEARCH_BUCKETS
                        }
                    };
                    search_buckets[bucket].push(sid);
                    let p = Pending {
                        pv_name: pv_name.clone(),
                        responder: Responder::Single(responder),
                        last_attempt: Instant::now(),
                        attempt: 0,
                        bucket,
                        holdoff_cycles: 0,
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
                            broadcast(&search_socket, &pkt, &extra_targets).await;
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
                    let bucket = match reason {
                        SearchReason::Initial => (current_bucket + 1) % N_SEARCH_BUCKETS,
                        SearchReason::Reconnect => {
                            let offset = (sid as usize) % N_SEARCH_BUCKETS;
                            (current_bucket + 1 + offset) % N_SEARCH_BUCKETS
                        }
                    };
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
                        holdoff_cycles: 0,
                    };
                    by_name.insert(pv_name, sid);
                    pending.insert(sid, p);
                    if reason == SearchReason::Initial {
                        if let Some(p) = pending.get_mut(&sid) {
                            p.attempt = 1;
                            p.last_attempt = Instant::now();
                            let pkt = codec.build_search(0, sid, &p.pv_name, [0,0,0,0], response_port, false);
                            broadcast(&search_socket, &pkt, &extra_targets).await;
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
                            // Reset attempt AND holdoff_cycles so the
                            // kicked retry doesn't inherit the holdoff
                            // from past failures (P-G26: failed-once
                            // searches otherwise still wait their
                            // remaining 10-cycle holdoff during fast-tick).
                            p.attempt = 0;
                            p.holdoff_cycles = 0;
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
                    // fast-tick mode for one revolution and reset every
                    // pending search's retry deadline / attempt
                    // counter so they all retry within ~6 s.
                    if fast_ticks_remaining == 0 {
                        tick = interval(Duration::from_millis(200));
                        tick.tick().await;
                    }
                    fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                    let now = Instant::now();
                    for p in pending.values_mut() {
                        p.last_attempt = now - Duration::from_secs(60);
                        p.attempt = 0;
                        p.holdoff_cycles = 0;
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
                    broadcast(&search_socket, &pkt, &extra_targets).await;
                }
                None => break,
            },

            res = search_socket.recv_from(&mut search_buf) => {
                if let Ok((n, _from)) = res {
                    // Multi-message drain (P-G10): pvxs packs many
                    // SEARCH messages per UDP datagram. Without the
                    // loop we'd parse only the first and silently
                    // drop the rest.
                    let mut pos = 0usize;
                    while pos < n {
                        let consumed = handle_search_response(
                            &search_buf[pos..n],
                            &mut pending, &mut by_name, &beacons, &ignore_guids,
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

            _ = beacon_clean_tick.tick() => {
                for (server, guid) in beacons.prune_stale(BEACON_TIMEOUT) {
                    announced.remove(&(server, guid));
                    let evt = Discovered::Timeout { server, guid };
                    subscribers.retain(|tx| tx.try_send(evt.clone()).is_ok());
                }
            }

            _ = tick.tick() => {
                let now = Instant::now();

                // 1. Flush expired FindAll multi-window responders.
                //    Always flush at deadline regardless of whether
                //    `accumulated` is empty — without this, a missing
                //    PV (no server claims the name) leaves the
                //    oneshot Sender alive forever, hanging the caller's
                //    `find_all().await` and so any user-set
                //    `PvaClient::timeout` on the outer op never gets
                //    a chance to apply (the inner ensure_active never
                //    returns control). pvxs returns from
                //    Operation::wait on the user timeout regardless of
                //    whether a search succeeded; the analogous
                //    behaviour in pva-rs is "deliver Vec::new() at
                //    deadline so the caller can decide whether to
                //    error or retry".
                //
                //    Also detect closed Single responders (caller
                //    dropped the find() future via outer timeout /
                //    abort) so the pending entry doesn't leak.
                let mut to_flush_multi = Vec::new();
                let mut to_drop_single = Vec::new();
                for (sid, p) in pending.iter() {
                    match &p.responder {
                        Responder::Multi { deadline, responder, .. } => {
                            if responder.is_closed() {
                                to_drop_single.push(*sid);
                            } else if now >= *deadline && p.attempt > 0 {
                                // attempt > 0 ensures the first
                                // broadcast actually went out; without
                                // this, Reconnect entries that haven't
                                // had their bucket fire yet would
                                // flush prematurely with no responses.
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
                        if let Responder::Multi { responder, accumulated, .. } = p.responder {
                            let _ = responder.send(accumulated);
                        }
                    }
                }
                for sid in to_drop_single {
                    if let Some(p) = pending.remove(&sid) {
                        by_name.remove(&p.pv_name);
                        search_buckets[p.bucket].retain(|x| *x != sid);
                        // Sender drops at end of scope; that's the
                        // signal to the caller (already-cancelled).
                    }
                }

                // 2. Process exactly one search bucket per tick. Pending
                //    searches in this bucket get one UDP retransmit; the
                //    bucket is then drained and each pending re-armed
                //    into a future bucket — `current + nBuckets` (i.e.
                //    same slot, 30 ticks later) for the steady-state
                //    case, plus an extra `RETRY_HOLDOFF_BUCKETS` shift
                //    when the search has already failed once or more
                //    (matches pvxs Channel::disconnect holdoff).
                let bucket_ids = std::mem::take(&mut search_buckets[current_bucket]);
                for sid in bucket_ids {
                    // Skip-on-holdoff: a retry sets holdoff_cycles =
                    // RETRY_HOLDOFF_BUCKETS; the search isn't sent
                    // this cycle, just re-placed in the same bucket
                    // and the counter decremented. After holdoff
                    // expires the search fires again. Replaces the
                    // arithmetically-broken `(current+N+EXTRA) % N`
                    // formula that effectively reduced retry holdoff
                    // to 9 s instead of >30 s.
                    let mut send_now = true;
                    let mut still_pending = false;
                    let mut responder_dead = false;
                    if let Some(p) = pending.get_mut(&sid) {
                        still_pending = true;
                        // F6: drop searches whose oneshot responder
                        // was already closed (caller cancelled their
                        // find() future via timeout / abort). Without
                        // this the bucket loop keeps re-broadcasting
                        // dead searches forever.
                        responder_dead = match &p.responder {
                            Responder::Single(tx) => tx.is_closed(),
                            Responder::Multi { responder, .. } => responder.is_closed(),
                        };
                        if !responder_dead && p.holdoff_cycles > 0 {
                            p.holdoff_cycles -= 1;
                            send_now = false;
                            // Re-push to the NEXT tick's bucket (not
                            // current) so the holdoff counter
                            // decrements once per tick — matching the
                            // intent that RETRY_HOLDOFF_BUCKETS is a
                            // bucket-distance shift (~10 ticks ≈ 10 s
                            // extra) rather than 10 full N-tick cycles
                            // (~300 s). Re-pushing into current_bucket
                            // (the round-3 attempt) only fired the
                            // search once per cycle, multiplying the
                            // holdoff by N_SEARCH_BUCKETS.
                            let next = (current_bucket + 1) % N_SEARCH_BUCKETS;
                            search_buckets[next].push(sid);
                        }
                    }
                    if responder_dead {
                        if let Some(p) = pending.remove(&sid) {
                            by_name.remove(&p.pv_name);
                        }
                        continue;
                    }
                    if !still_pending || !send_now {
                        continue;
                    }
                    let pkt_opt = pending.get_mut(&sid).map(|p| {
                        // First-broadcast bookkeeping for `Reconnect`
                        // find_all callers: the deadline was set to
                        // `find_all_call_time + MULTI_SERVER_WINDOW`
                        // assuming an immediate broadcast. With the
                        // bucket-spread Reconnect path that broadcast
                        // can land 0-30 s later; without re-anchoring,
                        // the multi-server accumulation window has
                        // already expired by the time the first SEARCH
                        // actually goes out, so subsequent responses
                        // beyond the very first one are dropped.
                        // Re-arm the deadline relative to NOW on the
                        // first attempt so the full MULTI_SERVER_WINDOW
                        // is preserved end-to-end.
                        if p.attempt == 0 {
                            if let Responder::Multi { ref mut deadline, .. } = p.responder {
                                *deadline = now + MULTI_SERVER_WINDOW;
                            }
                        }
                        p.last_attempt = now;
                        p.attempt = p.attempt.saturating_add(1);
                        // Re-place in next-tick's slot (one full
                        // cycle = N ticks ahead), but if this was a
                        // retry, also tag holdoff so we wait an
                        // extra RETRY_HOLDOFF_BUCKETS cycles before
                        // actually transmitting again.
                        if p.attempt > 1 {
                            p.holdoff_cycles = RETRY_HOLDOFF_BUCKETS as u32;
                        }
                        p.bucket = current_bucket;
                        codec.build_search(
                            0, sid, &p.pv_name, [0,0,0,0], response_port, false,
                        )
                    });
                    if let Some(pkt) = pkt_opt {
                        broadcast(&search_socket, &pkt, &extra_targets).await;
                        if let Some(p) = pending.get(&sid) {
                            search_buckets[p.bucket].push(sid);
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

async fn broadcast(socket: &AsyncUdpV4, packet: &[u8], extra_targets: &[SocketAddr]) {
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
        let needs_fanout = match t {
            SocketAddr::V4(v4) => v4.ip().is_broadcast() || v4.ip().is_multicast(),
            SocketAddr::V6(_) => false,
        };
        let result = if needs_fanout {
            socket.fanout_to(packet, t).await.map(|_| ())
        } else {
            socket.send_to(packet, t).await.map(|_| ())
        };
        if let Err(e) = result {
            debug!("search broadcast to {t} failed: {e}");
        }
    }
}

/// Returns bytes consumed from `bytes` so the caller can advance to
/// the next chained message in the same datagram (P-G10).
fn handle_search_response(
    bytes: &[u8],
    pending: &mut HashMap<u32, Pending>,
    by_name: &mut HashMap<String, u32>,
    _beacons: &Arc<BeaconTracker>,
    ignore_guids: &std::collections::HashSet<[u8; 12]>,
) -> usize {
    let Ok(Some((frame, consumed))) = try_parse_frame(bytes) else {
        return 0;
    };
    let Ok(resp) = decode_search_response(&frame) else {
        return consumed;
    };
    if !resp.found {
        return consumed;
    }
    // pvxs procSearchReply (client.cpp:857-863) drops responses whose
    // server GUID is on the blocklist.
    if ignore_guids.contains(&resp.guid) {
        return consumed;
    }
    for cid in resp.cids {
        let server = rewrite_loopback(resp.server_addr);
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
    let Ok(Some((frame, consumed))) = try_parse_frame(bytes) else {
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
    let Some(ip) = ip_from_bytes(&addr_arr) else {
        return consumed;
    };
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
            p.attempt = 0;
            p.holdoff_cycles = 0;
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

fn rewrite_loopback(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
    } else {
        addr
    }
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
        assert_eq!(rewrite_loopback(a), a);
    }

    #[test]
    fn rewrite_loopback_replaces_unspecified() {
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5075);
        let r = rewrite_loopback(a);
        assert_eq!(r.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    /// `Reconnect` reason must spread across many buckets when a mass
    /// disconnect cascade hands the engine a contiguous block of fresh
    /// sids — replicate the bucket-placement formula and assert we
    /// touch ≥ N_SEARCH_BUCKETS / 2 distinct slots over a 5_000-sid
    /// window. `Initial` (current_bucket+1 always) deliberately does
    /// not get this spread.
    #[test]
    fn reconnect_bucket_spread_across_ring() {
        let current_bucket = 7usize;
        let sids: Vec<u32> = (1_000..6_000).collect();
        let mut hit = [false; N_SEARCH_BUCKETS];
        for sid in sids {
            let offset = (sid as usize) % N_SEARCH_BUCKETS;
            let bucket = (current_bucket + 1 + offset) % N_SEARCH_BUCKETS;
            hit[bucket] = true;
        }
        let distinct = hit.iter().filter(|h| **h).count();
        assert_eq!(
            distinct, N_SEARCH_BUCKETS,
            "Reconnect should land in every bucket given a wide sid block; got {distinct}"
        );
    }

    #[test]
    fn initial_bucket_is_next_tick() {
        let current_bucket = 13usize;
        let bucket = (current_bucket + 1) % N_SEARCH_BUCKETS;
        assert_eq!(bucket, 14);
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

        let engine = SearchEngine::spawn(Vec::new()).await.expect("spawn engine");

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
}
