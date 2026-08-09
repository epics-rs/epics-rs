//! Per-PV channel state machine.
//!
//! A [`Channel`] is the long-lived handle through which ops (GET / PUT /
//! MONITOR / RPC) reach a server. Internally:
//!
//! ```text
//!   Idle
//!     │  ensure_active()
//!     ▼
//!   Searching ────► Connecting ────► Active
//!     ▲                                 │
//!     │  ServerConn closed              │
//!     └─────────────────────────────────┘
//! ```
//!
//! Multiple ops can ride on the same channel concurrently: each gets a
//! fresh `ioid` and registers with the underlying [`ServerConn`] router.
//! Reconnect is **automatic** and transparent to monitor consumers — see
//! [`crate::client_native::ops_v2::op_monitor_handle`] for the loop that
//! re-issues INIT/START on each new server conn.

// (1 search-timeout test gated out feature-ON below; §4.2 UDP search, stage 3.)

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;
use tokio::sync::{Mutex, Notify};

use crate::error::{PvaError, PvaResult};

use super::beacon_throttle::BeaconTracker;
use super::search_engine::SearchEngine;
use super::server_conn::{ConnConfig, ServerConn};

// pvxs seeds each ID namespace from a distinct non-zero base (commit
// 3b641bed) so a misused id fails loudly instead of aliasing a live one.
// CID base = pvxs `clientimpl.h:263` `nextCID=0x12345678` (pvxs reuses the
// CID as the channel's searchID, `clientimpl.h:181`).
static NEXT_CID: AtomicU32 = AtomicU32::new(0x1234_5678);

#[derive(Clone)]
pub enum ChannelState {
    Idle,
    Searching,
    Connecting,
    Active {
        server: Arc<ServerConn>,
        sid: u32,
        /// GUID expected for this server, captured from the
        /// SEARCH_RESPONSE that resolved the address. on
        /// reconnect via beacon-poke, we compare this against the
        /// current `BeaconTracker` view; if a different GUID is
        /// observed at the same address (server replacement at the
        /// same host:port within the channel's reconnect window) we
        /// log a warning and invalidate — the next ensure_active
        /// will re-search instead of reconnecting blind.
        expected_guid: Option<[u8; 12]>,
    },
    Closed,
}

/// A resolution candidate: a server address plus the GUID that resolved
/// it, when known. Search hits carry `Some(guid)` from the resolving
/// `SEARCH_RESPONSE`; direct-connect resolvers carry `None` (no search,
/// hence no reply GUID — matching pvxs forced-server, which also has no
/// channel GUID). `ChannelState::Active::expected_guid` is taken from this
/// `guid`, NOT re-derived from the beacon tracker.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    addr: std::net::SocketAddr,
    guid: Option<[u8; 12]>,
}

pub struct Channel {
    pub pv_name: String,
    pub cid: u32,
    state: RwLock<ChannelState>,
    /// Serializes state transitions so concurrent ops don't open multiple
    /// connections.
    transition_lock: Mutex<()>,
    /// Pulsed whenever the state changes. Monitor loops await this to learn
    /// of disconnect / reconnect.
    pub state_changed: Notify,
    user: String,
    host: String,
    op_timeout: std::time::Duration,
    /// TCP idle timeout threaded through to `ConnectionPool::get_or_connect`
    /// and ultimately into the per-connection heartbeat task.
    /// pvxs `effective.tcpTimeout` (clientconn.cpp:73-74).
    tcp_timeout: std::time::Duration,
    /// Shared connection pool (so multiple channels to the same server
    /// share a single TCP virtual circuit).
    pool: Arc<ConnectionPool>,
    resolver: Resolver,
    /// Alternative server addresses cached from the most recent search
    /// (excluding the one currently being tried). Multi-server failover:
    /// if `ensure_active` fails to connect or `CREATE_CHANNEL`s to the
    /// first server, it pops the next alternative before falling back to
    /// a fresh UDP search.
    alternatives: parking_lot::Mutex<Vec<Candidate>>,
    /// Earliest instant at which `ensure_active` is allowed to attempt a
    /// fresh connect, or `None` when reconnect may proceed immediately.
    /// Set per pvxs failure class (see `reconnect_holdoff`): a
    /// Connecting-stage TCP failure arms the fixed 10-bucket reconnect
    /// holdoff (pvxs `Channel::disconnect`, client.cpp:156-165, pushed via
    /// :206-214), a searched `CREATE_CHANNEL` refusal arms nothing (pvxs
    /// sets the channel back to Searching in the current bucket,
    /// clientconn.cpp:368-378), and a direct/forced-server refusal arms the
    /// fixed holdoff to stand in for pvxs "wait for reconnect"
    /// (clientconn.cpp:379-385). No Rust-only exponential counter is carried
    /// across these pvxs-distinct transitions.
    holdoff_until: parking_lot::Mutex<Option<std::time::Instant>>,
    /// Set to true by `ServerConn::route_frame` when a server-initiated
    /// `CMD_DESTROY_CHANNEL` arrives for this channel's current SID.
    /// `is_active` consults the flag so the next `ensure_active` falls
    /// through to a fresh search even though the cached
    /// `ChannelState::Active` says otherwise. Reset on every successful
    /// Active transition. pvxs e668038 "client track opByIOID per
    /// channel" parity — without it monitor streams silently hang
    /// after a server-side SharedPV close.
    server_destroyed: Arc<std::sync::atomic::AtomicBool>,
    /// Pulsed alongside `server_destroyed` to wake `wait_until_inactive`
    /// even when no other state transition has occurred.
    server_destroyed_notify: Arc<Notify>,
    /// `(sid, server)` we last registered with `ServerConn::register_sid_close`.
    /// Used to unregister on transitions out of Active so the router map
    /// doesn't accumulate stale (flag, notify) pairs.
    last_close_registration: parking_lot::Mutex<Option<(u32, Arc<ServerConn>)>>,
    /// Latched on the first successful Active transition. Distinguishes
    /// a fresh `find()` from a reconnect re-search so the search engine
    /// can pick `SearchReason::Initial` (immediate broadcast + place at
    /// `current_bucket+1` for fast single-channel latency) vs
    /// `SearchReason::Reconnect` (place at `current_bucket`, no
    /// immediate fire — pvxs `Channel::disconnect` parity). pvxs /
    /// ca-rs parity.
    has_been_active: std::sync::atomic::AtomicBool,
    /// Warm-GET fast path: cache the (sid, ioid, intro, slot) of a
    /// successfully completed default `op_get` so subsequent calls on
    /// the same channel can skip INIT and reuse the server-side
    /// introspection binding. Lazily invalidated when `ensure_active`
    /// returns a different (server, sid) — no need to hook the state
    /// transition path. See `op_get_inner`.
    pub(crate) cached_get: parking_lot::Mutex<Option<CachedGet>>,
}

/// Server-side state cached after the first successful default GET
/// against a channel. Lets subsequent GETs skip INIT — saves one
/// round-trip per call (~50µs localhost).
pub(crate) struct CachedGet {
    pub(crate) server: std::sync::Weak<ServerConn>,
    pub(crate) sid: u32,
    pub(crate) ioid: u32,
    pub(crate) intro: Arc<crate::pvdata::FieldDesc>,
    pub(crate) slot:
        Arc<parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<super::decode::Frame>>>>,
}

/// How a channel resolves its PV name to a server address.
enum Resolver {
    /// Use the SearchEngine — full UDP search + retry + beacon listener.
    Search(SearchEngine),
    /// Connect directly to a known address (used by `PvaClientBuilder::server_addr`).
    Direct(std::net::SocketAddr),
}

impl Resolver {
    /// Return the most recent GUID the SearchEngine's BeaconTracker
    /// has observed at `addr`, or None for direct-connect resolvers
    /// (we never learn a GUID for a hard-coded address). Used by
    /// `ChannelState::Active::expected_guid` to detect server
    /// replacement at the same address.
    fn last_guid_for(&self, addr: std::net::SocketAddr) -> Option<[u8; 12]> {
        match self {
            Resolver::Search(se) => se.beacon_guid_for(addr),
            Resolver::Direct(_) => None,
        }
    }
}

/// Pool of live `ServerConn`s, keyed by server address.
///
/// Optionally configured with a TLS client config — when present, every
/// new connection is upgraded to TLS via `pvas://` semantics.
#[derive(Default)]
pub struct ConnectionPool {
    inner: parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Arc<ServerConn>>>,
    /// Single-flight gate: per-address async mutex held for the duration
    /// of a `ServerConn::connect`. Two concurrent `get_or_connect` calls
    /// for the same `addr` serialize on this lock — the first dials, the
    /// second waits and then reuses the cached connection. Without it,
    /// both callers opened a real TCP connection and the race loser
    /// dropped its `Arc<ServerConn>`; since `ServerConn` has no Drop
    /// that cancels its tasks, the redundant socket and its
    /// reader/writer/heartbeat tasks leaked until idle timeout.
    connecting: parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Arc<Mutex<()>>>>,
    tls: parking_lot::Mutex<Option<Arc<crate::auth::TlsClientConfig>>>,
    /// Optional opt-in cap on a single inbound message's payload length,
    /// threaded into every `ServerConn` this pool dials. `None` (the
    /// default) means **unbounded** — pvxs keeps no client-side RX cap,
    /// and the streaming reader stays bounded by incremental 4 KiB reads
    /// plus the `op_timeout` deadline regardless. `Some(n)` rejects (and
    /// drops) any server header announcing more than `n` bytes.
    max_message_size: parking_lot::Mutex<Option<usize>>,
    /// Set by `PvaClient::close` (pvxs `Context::close`). Once true,
    /// reconnect attempts (especially the name-server fallback in
    /// `Channel::connect`) MUST refuse to dial — pvxs commit
    /// 4d12da87205e adds the same gate on the C++ side. Without it,
    /// an in-flight operation tearing down can spawn fresh
    /// connections during shutdown and leak the search-engine task.
    shutdown: std::sync::atomic::AtomicBool,
}

/// RAII guard for the single-flight gate slot in
/// [`ConnectionPool::connecting`]. Created by the dialer that owns the
/// per-address gate; on drop it removes the `addr` entry from
/// `connecting` — but only if the entry is still the exact gate this
/// guard owns (`Arc::ptr_eq`), so it never evicts a slot a later
/// dialer installed.
///
/// The guard runs on every exit path of the dialing block: normal
/// return, `?` early return, and a panic inside `ServerConn::connect`.
/// That panic-safety is the point — without it a panicking dial leaked
/// the `Arc<Mutex<()>>` entry permanently and every future caller for
/// `addr` serialized on a dead gate.
struct RemoveSlotOnDrop<'a> {
    pool: &'a ConnectionPool,
    addr: std::net::SocketAddr,
    gate: &'a Arc<Mutex<()>>,
}

impl Drop for RemoveSlotOnDrop<'_> {
    fn drop(&mut self) {
        let mut g = self.pool.connecting.lock();
        if let Some(current) = g.get(&self.addr) {
            if Arc::ptr_eq(current, self.gate) {
                g.remove(&self.addr);
            }
        }
    }
}

impl ConnectionPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enable TLS for every subsequent connect call.
    pub fn set_tls(&self, tls: Option<Arc<crate::auth::TlsClientConfig>>) {
        *self.tls.lock() = tls;
    }

    /// set the opt-in inbound message-size cap applied to
    /// every subsequent connect call. `None` (the default) is unbounded.
    pub fn set_max_message_size(&self, cap: Option<usize>) {
        *self.max_message_size.lock() = cap;
    }

    /// per-connection `(peer, bytes_rx, bytes_tx, alive, channels)`
    /// snapshot for `PvaClient::report`, where `channels` is the
    /// per-channel `(name, sid, bytes_rx, bytes_tx)` list pvxs copies into
    /// each `Report::Connection::channels` (client.cpp:495-496). When `zero`
    /// is true both the connection and the per-channel counters are reset
    /// after the read (pvxs `report(bool zero)` delta semantics).
    #[allow(clippy::type_complexity)]
    pub fn connection_byte_reports(
        &self,
        zero: bool,
    ) -> Vec<(
        std::net::SocketAddr,
        u64,
        u64,
        bool,
        Vec<(String, u32, u64, u64)>,
    )> {
        self.inner
            .lock()
            .iter()
            .map(|(addr, c)| {
                let (rx, tx) = c.byte_counters(zero);
                (*addr, rx, tx, c.is_alive(), c.channel_reports(zero))
            })
            .collect()
    }

    /// The TLS config currently in effect, if any. Used when deriving
    /// a credential-variant client (`PvaClient::with_asserted_identity`)
    /// so the new client reaches the same upstream over the same
    /// transport.
    pub fn tls(&self) -> Option<Arc<crate::auth::TlsClientConfig>> {
        self.tls.lock().clone()
    }

    pub async fn get_or_connect(
        self: &Arc<Self>,
        addr: std::net::SocketAddr,
        user: &str,
        host: &str,
        op_timeout: std::time::Duration,
        tcp_timeout: std::time::Duration,
    ) -> PvaResult<Arc<ServerConn>> {
        // Closed-context gate (pvxs 4d12da87205e): once `close()` /
        // `clear()` has run, refuse to dial. This is the dial-boundary
        // half of the single shutdown owner — without it an in-flight
        // operation tearing down (or a name-server reconnect path) could
        // still open a fresh socket after the context was closed, and
        // the channel-factory gate alone would be a false invariant.
        if self.is_shutdown() {
            return Err(PvaError::Protocol("context closed".into()));
        }
        // Fast path: existing alive conn.
        {
            let map = self.inner.lock();
            if let Some(conn) = map.get(&addr).cloned() {
                if conn.is_alive() {
                    return Ok(conn);
                }
            }
        }
        // Single-flight: acquire (or create) the per-address gate and
        // hold it across the dial so concurrent callers for `addr` open
        // exactly one TCP connection. Without it both callers dialed and
        // the race loser dropped its `Arc<ServerConn>` — but `ServerConn`
        // has no Drop cancelling its tasks, so the redundant socket and
        // its reader/writer/heartbeat tasks leaked until idle timeout.
        //
        // The slot must be re-resolved on EVERY iteration: a peer dialer
        // that owns the slot removes it from `connecting` only after it
        // has published its result to `inner` (or failed). A late caller
        // arriving in that window must observe either the freshly cached
        // connection or the still-present gate — never a removed slot
        // that lets it start a second concurrent dial. Looping closes
        // that churn window: each pass re-checks `inner`, then takes
        // whatever gate is current.
        loop {
            // Acquire (or create) the per-address gate slot. We clone the
            // `Arc<Mutex<()>>` while holding the `connecting` lock, then
            // release `connecting` before awaiting the async gate mutex —
            // a parking_lot guard must never be held across `.await`.
            let gate = {
                let mut g = self.connecting.lock();
                g.entry(addr).or_default().clone()
            };
            let dialing = gate.lock().await;

            // Re-check under the gate: a peer caller may have just
            // connected and published to `inner`.
            {
                let map = self.inner.lock();
                if let Some(conn) = map.get(&addr).cloned() {
                    if conn.is_alive() {
                        // A peer dialer owns this gate slot; it will
                        // remove it from `connecting`. We must not.
                        return Ok(conn);
                    }
                }
            }

            // The slot we acquired may be a stale gate that a previous
            // owning dialer has already removed from `connecting`
            // (it removes after publishing/failing). If so, a NEW slot
            // now lives under `addr` and a fresh dial is in flight on it
            // — loop and contend on that one instead of dialing on a
            // detached gate, which would double-dial.
            {
                let g = self.connecting.lock();
                match g.get(&addr) {
                    Some(current) if Arc::ptr_eq(current, &gate) => {
                        // We own the live slot for `addr`. Fall through
                        // and dial; `RemoveSlotOnDrop` guarantees the
                        // slot is removed even on panic / early return.
                    }
                    _ => {
                        // Stale gate (removed, or replaced by a newer
                        // dialer's slot). Release and retry.
                        drop(g);
                        drop(dialing);
                        continue;
                    }
                }
            }

            // RAII guard: removes our owned slot from `connecting` on
            // any exit path — normal return, `?` early return, or a
            // panic inside `ServerConn::connect`. Without it a panicking
            // dial leaked the `Arc<Mutex<()>>` entry permanently and
            // every future caller for `addr` serialized on a dead gate.
            let _slot_guard = RemoveSlotOnDrop {
                pool: self,
                addr,
                gate: &gate,
            };

            // Drop dead entry and connect fresh.
            {
                let mut map = self.inner.lock();
                if let Some(conn) = map.get(&addr) {
                    if !conn.is_alive() {
                        map.remove(&addr);
                    }
                }
            }
            let tls = self.tls.lock().clone();
            let conn_config = ConnConfig {
                op_timeout,
                tcp_timeout,
                max_message_size: *self.max_message_size.lock(),
            };
            let connect_result = match tls {
                // Without the `tls` feature `TlsClientConfig` is uninhabited,
                // so this arm cannot be reached — and is not compiled. The
                // `None` arm below is then already exhaustive.
                #[cfg(feature = "tls")]
                Some(cfg) => {
                    ServerConn::connect_tls(
                        addr,
                        &addr.ip().to_string(),
                        cfg,
                        user,
                        host,
                        conn_config,
                    )
                    .await
                }
                #[cfg(not(feature = "tls"))]
                Some(cfg) => match *cfg {},
                None => ServerConn::connect(addr, user, host, conn_config).await,
            };
            let fresh = connect_result?;
            let mut map = self.inner.lock();
            // The gate serialized dialing; still prefer an alive existing
            // one in case a dead entry was re-inserted between the
            // re-check and here.
            if let Some(existing) = map.get(&addr).cloned() {
                if existing.is_alive() {
                    return Ok(existing);
                }
            }
            map.insert(addr, fresh.clone());
            // `_slot_guard` removes the gate slot from `connecting` here,
            // after the connection is visible in `inner`, so a caller
            // arriving next either finds the cached conn or — if it
            // already cloned this gate — loops and sees the slot gone.
            return Ok(fresh);
        }
    }

    pub fn close_dead(&self) {
        let mut map = self.inner.lock();
        map.retain(|_, conn| conn.is_alive());
    }

    /// Drop the cached connection for `addr` regardless of liveness
    ///. Called when a GUID mismatch is detected at the same
    /// address — the previous code cleared its own Channel state but
    /// left the pool entry, so subsequent channels resolving to the
    /// same addr re-used the stale (wrong-GUID) ServerConn.
    pub fn invalidate(&self, addr: SocketAddr) {
        self.inner.lock().remove(&addr);
    }

    /// Terminal teardown: close and drop every cached connection. Used by
    /// `PvaClient::close` for explicit shutdown. Also flips the shutdown
    /// flag so any subsequent `get_or_connect` / name-server reconnect path
    /// returns an error instead of dialing out (pvxs 4d12da87205e).
    ///
    /// Each connection is `close()`d, not merely dropped: a live operation
    /// handle — e.g. a monitor's subscription state, which holds
    /// `(Arc<ServerConn>, sid, ioid)` — keeps its own `Arc`, so dropping
    /// the map's `Arc` alone would leave the reader/writer tasks running
    /// and the monitor receiving data until idle timeout or peer
    /// disconnect. `close()` cancels the connection token, which drains the
    /// router (drops every per-ioid sender) so active monitor streams wake
    /// with `None`. Mirrors pvxs `Connection::cleanup()` resetting the
    /// socket on context close (clientconn.cpp:176-204).
    pub fn clear(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        // Drain under the lock, then close after releasing it so a
        // connection teardown can never re-enter the pool while held.
        let conns: Vec<Arc<ServerConn>> = self.inner.lock().drain().map(|(_, c)| c).collect();
        for conn in conns {
            conn.close();
        }
    }

    /// `true` after `clear()` (i.e. after `PvaClient::close`) has run.
    /// Channel reconnect paths consult this to skip name-server
    /// fallback during shutdown.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// pvxs `Channel::disconnect` applies a fixed 10-bucket reconnect holdoff
/// for a Connecting-stage TCP failure; with a 1 s bucket interval that is
/// ~10 s (client.cpp:156-165, pushed onto the ring at :206-214). We reuse
/// the same constant for the direct/forced-server refusal case, which pvxs
/// resolves by waiting for reconnect rather than fast re-searching
/// (clientconn.cpp:379-385).
const RECONNECT_HOLDOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// How a single candidate attempt failed inside `ensure_active`. pvxs
/// paces reconnect differently for a Connecting-stage TCP failure vs a
/// `CREATE_CHANNEL` refusal, so the two must not collapse into one
/// exponential counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    /// TCP connect failed before `CREATE_CHANNEL` was sent.
    Connect,
    /// TCP connected but the server's `CREATE_CHANNEL` response was an
    /// error (or the circuit dropped mid-create).
    CreateRefusal,
}

/// Map a candidate-loop failure to the reconnect holdoff pvxs would apply,
/// replacing the previous Rust-only `2^n` ladder armed on every failure:
///
/// - Connecting-stage TCP failure → fixed 10-bucket reconnect placement
///   (pvxs client.cpp:156-165 / :206-214).
/// - searched-channel `CREATE_CHANNEL` refusal → no holdoff; pvxs sets the
///   channel back to Searching in the current bucket and the ≤1 s ring tick
///   paces the re-search (clientconn.cpp:368-378).
/// - direct/forced-server `CREATE_CHANNEL` refusal → fixed holdoff; pvxs
///   logs and waits for reconnect with no search ring (clientconn.cpp:379-385),
///   so a fixed delay stands in for "wait for reconnect" and keeps the
///   pull-driven reconnect caller from hot-spinning where no bucket tick
///   exists to pace it.
fn reconnect_holdoff(class: Option<FailureClass>, is_direct: bool) -> Option<std::time::Duration> {
    match class {
        Some(FailureClass::Connect) => Some(RECONNECT_HOLDOFF),
        Some(FailureClass::CreateRefusal) if is_direct => Some(RECONNECT_HOLDOFF),
        Some(FailureClass::CreateRefusal) => None,
        // No classified failure (e.g. empty candidate set already returned
        // earlier) — apply the conservative connect-stage holdoff.
        None => Some(RECONNECT_HOLDOFF),
    }
}

/// Whether an exhausted candidate batch should re-enter the search ring
/// instead of surfacing the error to the waiting operation.
///
/// pvxs treats a `CREATE_CHANNEL` refusal on a *searched* channel as a state
/// transition back to `Searching`: it re-pushes the channel into
/// `searchBuckets[currentBucket]` and the operation stays pending until a
/// server accepts or the caller's own deadline ends it
/// (clientconn.cpp:368-378). That applies only when the channel is searched
/// (not direct) and every candidate failed at the CREATE stage — a
/// Connecting-stage TCP failure instead keeps the fixed reconnect holdoff
/// (clientconn.cpp:379-385) so an unreachable address is not hot-retried with
/// no pacing.
fn refusal_reenters_search(
    last_failure: Option<FailureClass>,
    saw_connect_failure: bool,
    is_direct: bool,
) -> bool {
    !is_direct && !saw_connect_failure && last_failure == Some(FailureClass::CreateRefusal)
}

impl Channel {
    pub fn new(
        pv_name: String,
        user: String,
        host: String,
        op_timeout: std::time::Duration,
        tcp_timeout: std::time::Duration,
        pool: Arc<ConnectionPool>,
        search: SearchEngine,
    ) -> Self {
        Self {
            pv_name,
            cid: NEXT_CID.fetch_add(1, Ordering::Relaxed),
            state: RwLock::new(ChannelState::Idle),
            transition_lock: Mutex::new(()),
            state_changed: Notify::new(),
            user,
            host,
            op_timeout,
            tcp_timeout,
            pool,
            resolver: Resolver::Search(search),
            alternatives: parking_lot::Mutex::new(Vec::new()),
            holdoff_until: parking_lot::Mutex::new(None),
            server_destroyed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_destroyed_notify: Arc::new(Notify::new()),
            last_close_registration: parking_lot::Mutex::new(None),
            has_been_active: std::sync::atomic::AtomicBool::new(false),
            cached_get: parking_lot::Mutex::new(None),
        }
    }

    /// Construct a channel that targets a fixed server address (no UDP search).
    pub fn new_direct(
        pv_name: String,
        user: String,
        host: String,
        op_timeout: std::time::Duration,
        tcp_timeout: std::time::Duration,
        pool: Arc<ConnectionPool>,
        addr: std::net::SocketAddr,
    ) -> Self {
        Self {
            pv_name,
            cid: NEXT_CID.fetch_add(1, Ordering::Relaxed),
            state: RwLock::new(ChannelState::Idle),
            transition_lock: Mutex::new(()),
            state_changed: Notify::new(),
            user,
            host,
            op_timeout,
            tcp_timeout,
            pool,
            resolver: Resolver::Direct(addr),
            alternatives: parking_lot::Mutex::new(Vec::new()),
            holdoff_until: parking_lot::Mutex::new(None),
            server_destroyed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_destroyed_notify: Arc::new(Notify::new()),
            last_close_registration: parking_lot::Mutex::new(None),
            has_been_active: std::sync::atomic::AtomicBool::new(false),
            cached_get: parking_lot::Mutex::new(None),
        }
    }

    pub fn current_state(&self) -> ChannelState {
        self.state.read().clone()
    }

    pub fn is_active(&self) -> bool {
        if self
            .server_destroyed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return false;
        }
        matches!(*self.state.read(), ChannelState::Active { ref server, .. } if server.is_alive())
    }

    /// Fast-path check to get the active server and sid without allocating
    /// async futures or timers. Used by op_get / op_put to avoid timeout
    /// overhead when the channel is already active.
    pub fn try_get_active(&self) -> Option<(Arc<ServerConn>, u32)> {
        let s = self.state.read();
        if let ChannelState::Active { server, sid, .. } = &*s {
            let destroyed = self
                .server_destroyed
                .load(std::sync::atomic::Ordering::Relaxed);
            if !destroyed && server.is_alive() {
                return Some((server.clone(), *sid));
            }
        }
        None
    }

    /// Notify pulsed by `route_frame` on server-initiated
    /// `CMD_DESTROY_CHANNEL`. External watchers (e.g. the
    /// `connect()` on-connect callback driver) await this alongside
    /// `state_changed` so they observe destroy events even when no
    /// other state transition fires.
    pub fn server_destroyed_notify(&self) -> &Notify {
        &self.server_destroyed_notify
    }

    pub fn close(&self) {
        // Route through `set_state` so the SID-close registration in
        // `ServerConn::router.by_sid_close` is unregistered as part
        // of leaving Active. A direct `state.write()` bypasses that
        // and would leak the entry until the connection itself dies.
        self.set_state(ChannelState::Closed);
    }

    /// Ensure the channel is in `Active` state, transitioning through
    /// Searching → Connecting as needed. Returns the live `(ServerConn, sid)`
    /// pair.
    pub async fn ensure_active(&self) -> PvaResult<(Arc<ServerConn>, u32)> {
        // Quick happy-path check. also verify that the
        // current server's GUID still matches the GUID we expected
        // for its address. If beacons report a different GUID at the
        // same address, the upstream server was replaced — drop the
        // cached state and fall through to a fresh search.
        // Server-initiated CMD_DESTROY_CHANNEL (pvxs e668038): the
        // route_frame handler sets `server_destroyed = true` when the
        // server tears down our SID; without this check the quick
        // path here would happily hand the dead SID back to the next
        // op, which the server then rejects with "unknown channel
        // sid" — the whole point of the destroyed-flag plumbing was
        // to avoid that round-trip.
        let mut force_research = false;
        {
            let s = self.state.read();
            if let ChannelState::Active {
                server,
                sid,
                expected_guid,
            } = &*s
            {
                let destroyed = self
                    .server_destroyed
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !destroyed && server.is_alive() {
                    let mismatched = match (
                        expected_guid.as_ref(),
                        self.resolver.last_guid_for(server.addr),
                    ) {
                        (Some(exp), Some(obs)) => exp != &obs,
                        _ => false,
                    };
                    if mismatched {
                        tracing::warn!(
                            addr = %server.addr,
                            "PVA server identity changed at same address; \
                             re-searching to validate channel"
                        );
                        force_research = true;
                    } else {
                        return Ok((server.clone(), *sid));
                    }
                } else if destroyed {
                    tracing::debug!(
                        sid = *sid,
                        addr = %server.addr,
                        "channel destroyed by server — re-searching"
                    );
                    force_research = true;
                }
            }
            if let ChannelState::Closed = &*s {
                return Err(PvaError::Protocol("channel closed".into()));
            }
        }
        if force_research {
            // also drop the stale pool entry so other channels
            // resolving to the same addr don't reuse the wrong-GUID
            // ServerConn until they too discover the mismatch.
            if let ChannelState::Active { server, .. } = &*self.state.read() {
                self.pool.invalidate(server.addr);
            }
            self.set_state(ChannelState::Idle);
            self.alternatives.lock().clear();
        }

        // Serialize transitions across concurrent callers.
        let _guard = self.transition_lock.lock().await;

        // Connect-fail holdoff. After a recent connect/CreateChannel
        // failure we sleep for the remainder of the holdoff window
        // before re-issuing the search. pvxs Channel::disconnect
        // (client.cpp:155-163) implements the same idea with a
        // 10-bucket future-push on the search ring; here we
        // accumulate `2^min(fails-1, 4)` seconds (cap 16s) per
        // consecutive failure. Reset to zero on the next successful
        // Active transition.
        let now = std::time::Instant::now();
        let wait = {
            let mut h = self.holdoff_until.lock();
            match *h {
                Some(t) if t > now => Some(t - now),
                _ => {
                    *h = None;
                    None
                }
            }
        };
        if let Some(d) = wait {
            epics_base_rs::runtime::task::sleep(d).await;
        }

        // Re-check after acquiring the lock.
        {
            let s = self.state.read();
            if let ChannelState::Active { server, sid, .. } = &*s {
                let destroyed = self
                    .server_destroyed
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !destroyed && server.is_alive() {
                    return Ok((server.clone(), *sid));
                }
            }
            if let ChannelState::Closed = &*s {
                return Err(PvaError::Protocol("channel closed".into()));
            }
        }

        let is_direct = matches!(self.resolver, Resolver::Direct(_));
        // A searched CREATE_CHANNEL refusal is a channel-state transition (back
        // to Searching), not a terminal operation error: pvxs re-pushes the
        // channel into searchBuckets[currentBucket] and the waiting operation
        // stays pending until a server accepts or the caller's own deadline
        // ends it (clientconn.cpp:368-378). Mirror that by re-entering the
        // search ring and retrying here; the op-level timeout in
        // ops_v2::ensure_active_with_op_timeout (and SubscriptionHandle drop
        // for monitors) owns the user-visible deadline.
        let mut researched_after_refusal = false;
        loop {
            // Pull a candidate server. Prefer cached alternatives from the
            // most recent multi-window search; otherwise issue a fresh search.
            // The lock guard from parking_lot is !Send, so we drop it before
            // any await.
            let cached: Option<Vec<Candidate>> = {
                let mut alts = self.alternatives.lock();
                if alts.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut *alts))
                }
            };
            let candidates = match cached {
                Some(list) => list,
                None => {
                    self.set_state(ChannelState::Searching);
                    // Pick `Reconnect` once we've ever been Active so the
                    // search lands in the current bucket and waits for the
                    // next 1 Hz tick (pvxs `Channel::disconnect` parity);
                    // otherwise this is a fresh resolve and `Initial`
                    // earns the immediate broadcast for fast first-attempt
                    // latency.
                    let reason = if researched_after_refusal {
                        // A refused CREATE_CHANNEL usually recurs on retry, so
                        // the re-search parks in the furthest future bucket —
                        // one ring revolution, ~30 s — instead of riding the
                        // ≤1 s tick (pvxs 084336bb, `clientconn.cpp:376-381`).
                        // With the previous Reconnect placement a server that
                        // answered SEARCH instantly and then refused CREATE was
                        // re-asked about once a second, forever.
                        super::search_engine::SearchReason::CreateRefused
                    } else if self
                        .has_been_active
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        super::search_engine::SearchReason::Reconnect
                    } else {
                        super::search_engine::SearchReason::Initial
                    };
                    // Use single-server `find()` (delivers on first
                    // SEARCH_RESPONSE).
                    //
                    // - Initial: the engine broadcasts immediately on
                    //   receipt, so a healthy server replies in
                    //   microseconds, AND places the SEARCH at
                    //   `current_bucket+1` so a slow server is retried on
                    //   the pvxs-style `nSearch+1` escalation. No outer
                    //   timeout: a fresh resolve stays pending
                    //   until a server answers; the operation-level timeout
                    //   surfaces "no server" for one-shot ops (a wrong PV
                    //   name fails when the caller's op timeout elapses,
                    //   not at a hard-coded 200 ms ceiling).
                    //
                    // - Reconnect: NO outer timeout. The engine places
                    //   the SEARCH in the current bucket and the next
                    //   periodic tick (≤1 s) broadcasts it; if the
                    //   server doesn't reply, the engine retries on a
                    //   pvxs-style `nSearch+1`-bucket escalation
                    //   (1 s, 2 s, 3 s, ... up to 30 s) and a beacon
                    //   arrival from the recovered server kicks the
                    //   pending entries into fast-tick mode for
                    //   sub-second recovery. Mirrors pvxs's design,
                    //   where Channel::disconnect just leaves the
                    //   channel in `searchBuckets` — there is no
                    //   caller-facing find() with a timeout. Adding
                    //   one was a foot-gun: the previous `MULTI_SERVER_WINDOW`
                    //   ceiling cancelled the SEARCH before its bucket
                    //   could fire (dropped it as a zombie), and
                    //   recovery only happened when a beacon arrived
                    //   and a fresh retry cycle happened to align with
                    //   it. Without the timeout, the find() future
                    //   stays pending indefinitely; the monitor loop
                    //   awaits it (no CPU cost), and dropping the
                    //   `SubscriptionHandle` cancels everything via
                    //   normal future-drop semantics.
                    //
                    // Initial-symptom note (preserved): `pvget-rs PV`
                    // against a local IOC used to take ~1 s vs legacy
                    // `pvget`'s ~10 ms; root cause was `find_all`'s
                    // 1 Hz tick coupling to delivery, fixed by switching
                    // to single-server `find()`.
                    match &self.resolver {
                        Resolver::Search(engine) => {
                            // no inner timeout for either reason.
                            // Both `find()` calls stay pending until a
                            // SEARCH_RESPONSE arrives; the engine drives
                            // recovery via the bucket scheduler (Initial
                            // fires immediately + retries from
                            // `current_bucket+1`, Reconnect rides the next
                            // tick). The user-visible deadline is owned by
                            // the operation-level timeout
                            // (`ops_v2::ensure_active_with_op_timeout`) for
                            // one-shot ops, and by `SubscriptionHandle` drop
                            // for monitor loops — matching pvxs, where a
                            // newly opened channel lives in the search ring
                            // until the server answers or the caller drops
                            // the operation. The previous 200 ms
                            // `MULTI_SERVER_WINDOW` ceiling on `Initial`
                            // collapsed a slow-but-live search into "no
                            // servers found" and let the bucket loop reap the
                            // still-wanted pending entry as a zombie the
                            // moment the outer timeout closed the responder.
                            let result = engine.find(&self.pv_name, reason).await.ok();
                            match result {
                                Some(hit) => vec![Candidate {
                                    addr: hit.server,
                                    guid: Some(hit.guid),
                                }],
                                None => Vec::new(),
                            }
                        }
                        Resolver::Direct(addr) => vec![Candidate {
                            addr: *addr,
                            guid: None,
                        }],
                    }
                }
            };

            if candidates.is_empty() {
                return Err(PvaError::Protocol("no servers found for PV".into()));
            }

            // Try each candidate in order; stash the rest as alternatives.
            let mut last_err: Option<PvaError> = None;
            let mut last_failure: Option<FailureClass> = None;
            let mut saw_connect_failure = false;
            for (idx, cand) in candidates.iter().enumerate() {
                self.set_state(ChannelState::Connecting);
                match self
                    .pool
                    .get_or_connect(
                        cand.addr,
                        &self.user,
                        &self.host,
                        self.op_timeout,
                        self.tcp_timeout,
                    )
                    .await
                {
                    Err(e) => {
                        last_err = Some(e);
                        last_failure = Some(FailureClass::Connect);
                        saw_connect_failure = true;
                        continue;
                    }
                    Ok(server) => match self.do_create_channel(&server).await {
                        Ok(sid) => {
                            // Stash remaining candidates as alternatives.
                            let leftovers: Vec<_> =
                                candidates.iter().skip(idx + 1).copied().collect();
                            *self.alternatives.lock() = leftovers;
                            self.set_state(ChannelState::Active {
                                server: server.clone(),
                                sid,
                                // Capture the GUID the resolving
                                // SEARCH_RESPONSE carried for this PV
                                // (`cand.guid`), so the stored
                                // `expected_guid` is the identity of the
                                // server that actually claimed the name —
                                // pvxs `procSearchReply` parity, where
                                // `chan->guid` is set from the reply, not
                                // from a beacon. For a `Direct` resolver
                                // (no search) fall back to the beacon
                                // tracker's last GUID for the address. If a
                                // future reconnect observes a different
                                // GUID, the ensure_active path detects it.
                                expected_guid: cand
                                    .guid
                                    .or_else(|| self.resolver.last_guid_for(server.addr)),
                            });
                            // Successful Active — clear any pending reconnect
                            // holdoff; the next disconnect re-derives its pacing
                            // from its own failure class.
                            *self.holdoff_until.lock() = None;
                            return Ok((server, sid));
                        }
                        Err(e) => {
                            last_err = Some(e);
                            last_failure = Some(FailureClass::CreateRefusal);
                            continue;
                        }
                    },
                }
            }
            // Every candidate failed. A searched CREATE_CHANNEL refusal is a
            // channel-state transition back to Searching, not a terminal op error:
            // re-enter the search ring and keep the waiting operation pending
            // (pvxs clientconn.cpp:368-381). The next pass clears nothing extra —
            // `alternatives` is already drained — and issues a CreateRefused
            // search parked one full ring revolution out (pvxs 084336bb).
            if refusal_reenters_search(last_failure, saw_connect_failure, is_direct) {
                researched_after_refusal = true;
                continue;
            }
            // Otherwise pace the next attempt by the pvxs failure class (see
            // `reconnect_holdoff`) and surface the error: a Connecting-stage TCP
            // failure earns the fixed 10-bucket holdoff, and a direct-server
            // refusal earns the fixed holdoff in lieu of pvxs "wait for reconnect"
            // (clientconn.cpp:379-385) — a deliberate deviation, since a direct
            // channel has no search ring to re-enter and a tight retry against the
            // same refusing server must be avoided.
            *self.holdoff_until.lock() =
                reconnect_holdoff(last_failure, is_direct).map(|d| std::time::Instant::now() + d);
            return Err(last_err.unwrap_or_else(|| PvaError::Protocol("connect failed".into())));
        }
    }

    fn set_state(&self, new_state: ChannelState) {
        // Tear down any previous SID-close registration (server-side
        // CMD_DESTROY_CHANNEL hook). Always do this on state change so a
        // stale `(sid, server)` entry can't fire spuriously after we've
        // moved past the old SID.
        let prev_reg = self.last_close_registration.lock().take();
        if let Some((old_sid, old_server)) = prev_reg {
            old_server.unregister_sid_close(old_sid);
            // Drop the old SID's per-channel report counters too — the
            // channel is leaving that (server, sid) binding.
            old_server.unregister_channel(old_sid);
        }

        // Entering Active: clear the destroyed flag for the fresh SID and
        // register a new (flag, notify) pair with the new server.
        // Also latch `has_been_active = true` so subsequent re-searches
        // (after a Server disconnect / DESTROY_CHANNEL) tell the search
        // engine to use `SearchReason::Reconnect` bucket spreading
        // instead of the immediate-fire `Initial` path.
        if let ChannelState::Active {
            ref server, sid, ..
        } = new_state
        {
            self.server_destroyed
                .store(false, std::sync::atomic::Ordering::Relaxed);
            server.register_sid_close(
                sid,
                Arc::clone(&self.server_destroyed),
                Arc::clone(&self.server_destroyed_notify),
            );
            // Register the PV name under this SID so the connection can
            // attribute per-channel byte traffic for `PvaClient::report`
            // (pvxs `conn->chanBySID`, client.cpp:495).
            server.register_channel(sid, &self.pv_name);
            *self.last_close_registration.lock() = Some((sid, server.clone()));
            self.has_been_active
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        *self.state.write() = new_state;
        self.state_changed.notify_waiters();
    }

    async fn do_create_channel(&self, server: &Arc<ServerConn>) -> PvaResult<u32> {
        use super::decode::decode_create_channel_response;
        use crate::codec::PvaCodec;

        let big_endian = matches!(server.byte_order(), crate::proto::ByteOrder::Big);
        let codec = PvaCodec { big_endian };
        let req = codec.build_create_channel(self.cid, &self.pv_name);

        // Register a one-shot waiter for the CREATE_CHANNEL response.
        let waiter = server.register_cid_waiter(self.cid);
        server.send(req).await?;

        let frame = epics_base_rs::runtime::task::timeout(self.op_timeout, waiter)
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(|_| PvaError::Protocol("create_channel response cancelled".into()))?;

        let resp = decode_create_channel_response(&frame)?;
        if !resp.status.is_success() {
            return Err(PvaError::Protocol(format!(
                "create_channel({}) failed: {:?}",
                self.pv_name, resp.status
            )));
        }
        Ok(resp.sid)
    }

    /// Wait until the channel transitions out of its current `Active` state
    /// (i.e. the `ServerConn` died OR the server sent CMD_DESTROY_CHANNEL
    /// for our SID). Used by monitor loops to drive reconnect.
    pub async fn wait_until_inactive(&self) {
        loop {
            let state_n = self.state_changed.notified();
            let destroyed_n = self.server_destroyed_notify.notified();
            tokio::pin!(state_n);
            tokio::pin!(destroyed_n);
            // enable() registers the waiter eagerly, so a notify_waiters
            // that fires between the recheck and the await is captured.
            // Without it, a state transition firing in that window
            // leaves this loop blocked until the next transition.
            state_n.as_mut().enable();
            destroyed_n.as_mut().enable();
            if !self.is_active() {
                return;
            }
            tokio::select! {
                _ = state_n => {}
                _ = destroyed_n => {}
            }
        }
    }
}

// Used by tests / external code that wants to inspect throttle status.
impl Channel {
    pub fn beacon_tracker(&self) -> Option<Arc<BeaconTracker>> {
        match &self.resolver {
            Resolver::Search(engine) => Some(engine.beacons.clone()),
            Resolver::Direct(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> Channel {
        let pool = ConnectionPool::new();
        let addr: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();
        Channel::new_direct(
            "TEST:PV".into(),
            "u".into(),
            "h".into(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(40),
            pool,
            addr,
        )
    }

    /// pvxs reconnect pacing by failure class (client.cpp:156-165,
    /// clientconn.cpp:368-385): a Connecting-stage TCP failure earns the
    /// fixed 10-bucket holdoff; a searched `CREATE_CHANNEL` refusal earns
    /// none (the search ring tick paces it); a direct-server refusal earns
    /// the fixed holdoff. No exponential ladder across these transitions.
    #[test]
    fn reconnect_holdoff_matches_pvxs_failure_classes() {
        // Connecting-stage TCP failure → fixed 10-bucket holdoff,
        // independent of whether the resolver is searched or direct.
        assert_eq!(
            reconnect_holdoff(Some(FailureClass::Connect), false),
            Some(RECONNECT_HOLDOFF)
        );
        assert_eq!(
            reconnect_holdoff(Some(FailureClass::Connect), true),
            Some(RECONNECT_HOLDOFF)
        );
        // Searched CREATE_CHANNEL refusal → no holdoff (re-enter the
        // current search bucket; the ≤1 s ring tick paces the re-search).
        assert_eq!(
            reconnect_holdoff(Some(FailureClass::CreateRefusal), false),
            None
        );
        // Direct/forced-server CREATE_CHANNEL refusal → fixed holdoff in
        // lieu of pvxs "wait for reconnect" (no search ring to pace it).
        assert_eq!(
            reconnect_holdoff(Some(FailureClass::CreateRefusal), true),
            Some(RECONNECT_HOLDOFF)
        );
        // No classified failure → conservative connect-stage holdoff.
        assert_eq!(reconnect_holdoff(None, false), Some(RECONNECT_HOLDOFF));
    }

    #[test]
    fn refusal_reenters_search_only_for_pure_searched_create_refusal() {
        // Searched channel, every candidate refused CREATE → re-enter the
        // search ring (pvxs sets state=Searching, re-pushes into the bucket).
        assert!(refusal_reenters_search(
            Some(FailureClass::CreateRefusal),
            false,
            false
        ));
        // Direct channel refusal → no search ring; surface with holdoff.
        assert!(!refusal_reenters_search(
            Some(FailureClass::CreateRefusal),
            false,
            true
        ));
        // A Connecting-stage TCP failure anywhere in the batch → keep the
        // 10-bucket reconnect holdoff, do NOT hot-retry an unreachable addr.
        assert!(!refusal_reenters_search(
            Some(FailureClass::CreateRefusal),
            true,
            false
        ));
        assert!(!refusal_reenters_search(
            Some(FailureClass::Connect),
            true,
            false
        ));
        // No classified failure (empty candidate set already returned) →
        // not a refusal, do not loop.
        assert!(!refusal_reenters_search(None, false, false));
    }

    /// `close()` must route through `set_state` so the SID-close
    /// hook (`ServerConn::router.by_sid_close`) is unregistered when
    /// leaving Active. A direct `state.write()` would leak the entry
    /// until the connection itself dies (review finding #5).
    #[test]
    fn close_transitions_to_closed_via_set_state() {
        let ch = make_channel();
        assert!(matches!(*ch.state.read(), ChannelState::Idle));
        ch.close();
        assert!(matches!(*ch.state.read(), ChannelState::Closed));
        // ensure_active should now error.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(ch.ensure_active());
        assert!(matches!(
            res,
            Err(PvaError::Protocol(ref m)) if m.contains("closed")
        ));
    }

    /// pvxs 4d12da87205e — `ConnectionPool::clear` (called by
    /// `PvaClient::close`) must flip the shutdown flag so subsequent
    /// reconnect paths reject. Otherwise tearing down an in-flight
    /// operation can re-spawn fresh TCP dials to name-servers.
    #[test]
    fn connection_pool_clear_marks_shutdown() {
        let pool = ConnectionPool::new();
        assert!(!pool.is_shutdown());
        pool.clear();
        assert!(pool.is_shutdown());
    }

    /// `is_shutdown` is sticky — a fresh pool that has never been
    /// cleared returns false.
    #[test]
    fn connection_pool_fresh_is_not_shutdown() {
        let pool = ConnectionPool::new();
        assert!(!pool.is_shutdown());
    }

    /// `is_active()` must return `false` whenever `server_destroyed`
    /// is set, regardless of the cached `ChannelState::Active` —
    /// otherwise the quick path in `ensure_active` hands stale
    /// (server, sid) pairs back to the next op (review finding #1).
    #[test]
    fn is_active_observes_server_destroyed_flag() {
        let ch = make_channel();
        // Idle → not active regardless of flag.
        assert!(!ch.is_active());
        ch.server_destroyed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!ch.is_active(), "destroyed flag must keep is_active false");
        ch.server_destroyed
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!ch.is_active(), "still Idle, still not active");
    }

    /// Regression: a fresh channel's initial search must NOT
    /// fail with "no servers found" at the 200 ms `MULTI_SERVER_WINDOW`.
    /// The inner cap is gone; `ensure_active` stays pending until a
    /// SEARCH_RESPONSE arrives or the caller's operation-level timeout
    /// fires. We assert the failure lands at the *operation* timeout,
    /// not the old 200 ms ceiling.
    ///
    /// Companion to `search_engine::reconnect_find_does_not_complete_without_response`,
    /// which guards the same invariant for the `Reconnect` reason at the
    /// engine layer; this guards the `Initial` reason at the
    /// `ensure_active` layer where the cap actually lived.
    // Drives a search that never resolves and asserts the op-timeout owner
    // fires; the search engine's spawned tick `interval` now runs on the
    // reactor-less callback pool under `rtems-exec-model` (§4.2 UDP search is
    // deferred). Reactor-dependent — gated out feature-ON (stage 3).
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(epics_env)]
    async fn initial_search_failure_is_owned_by_op_timeout_not_200ms() {
        use std::time::Duration;

        // Suppress real broadcast so no stray SEARCH on the LAN can be
        // answered and resolve the channel out from under the test.
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free. `current_thread`
        // only constrains this test's own async executor, not the test
        // harness's cross-test thread parallelism — these vars ARE read
        // by production config code (config::env) and by other
        // epics_env-group tests.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }
        let engine =
            crate::client_native::search_engine::SearchEngine::spawn(Vec::new(), Vec::new())
                .await
                .expect("spawn engine");

        let op_timeout = Duration::from_millis(600);
        let ch = Channel::new(
            "PVAFR12:MISSING:PV".into(),
            "u".into(),
            "h".into(),
            op_timeout,
            Duration::from_secs(40),
            ConnectionPool::new(),
            engine,
        );

        // Drive ensure_active under the operation-level timeout, exactly
        // as `ops_v2::ensure_active_with_op_timeout` does for one-shot ops.
        let started = std::time::Instant::now();
        let res = tokio::time::timeout(op_timeout, ch.ensure_active()).await;
        let elapsed = started.elapsed();

        // `ServerConn` is not `Debug`, so summarize the outcome by hand.
        let outcome = match &res {
            Ok(Ok((conn, sid))) => format!("resolved to {} (sid {sid})", conn.addr),
            Ok(Err(_)) => "inner error (e.g. \"no servers found\")".to_string(),
            Err(_) => "timed out — still pending".to_string(),
        };

        // Pre-fix: ensure_active returned Ok(Err("no servers found")) at
        // ~200 ms, so the outer timeout never fired and `elapsed` was far
        // below `op_timeout`. Post-fix: find() stays pending, so the
        // outer timeout owns the failure at ~op_timeout.
        assert!(
            res.is_err(),
            "ensure_active resolved before the op timeout — the 200 ms \
             MULTI_SERVER_WINDOW initial-search ceiling was reintroduced; \
             outcome: {outcome}"
        );
        assert!(
            elapsed >= op_timeout,
            "initial search bailed early (elapsed {elapsed:?} < op_timeout \
             {op_timeout:?}) — failure must be owned by the operation \
             timeout, not the 200 ms window"
        );
    }

    /// BUG 3 regression: `ConnectionPool::get_or_connect` must
    /// single-flight concurrent callers for the same address. Two
    /// callers racing on the same `addr` must open exactly ONE TCP
    /// connection — the per-address gate serializes the dial. Before
    /// the fix both callers dialed concurrently and the race loser
    /// dropped a `ServerConn` whose reader/writer/heartbeat tasks and
    /// socket leaked (no Drop cancels them).
    ///
    /// The probe listener accepts and then stalls (never completes the
    /// PVA handshake), so each dial blocks until `op_timeout`. With
    /// single-flight the second `accept()` cannot happen while the
    /// first dial is still in flight; without it both accepts land
    /// within milliseconds of each other.
    #[tokio::test]
    async fn get_or_connect_single_flights_concurrent_callers() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe listener");
        let addr = listener.local_addr().expect("probe addr");

        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_srv = accepts.clone();
        // Accept loop: count every accepted connection, then hold the
        // socket so the client's handshake stalls until op_timeout.
        let _srv = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                accepts_srv.fetch_add(1, Ordering::SeqCst);
                held.push(sock); // keep alive, never reply
            }
        });

        let pool = ConnectionPool::new();
        let op_timeout = Duration::from_millis(400);
        let tcp_timeout = Duration::from_secs(40);

        // Two concurrent callers for the SAME addr.
        let p1 = pool.clone();
        let c1 = tokio::spawn(async move {
            p1.get_or_connect(addr, "u", "h", op_timeout, tcp_timeout)
                .await
        });
        let p2 = pool.clone();
        let c2 = tokio::spawn(async move {
            p2.get_or_connect(addr, "u", "h", op_timeout, tcp_timeout)
                .await
        });

        // Mid-first-dial: the gate must have blocked the second caller,
        // so only one connection has been accepted so far.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "single-flight gate must block the 2nd dial while the 1st is in flight"
        );

        // Both dials ultimately fail (handshake stalls → timeout).
        let r1 = c1.await.expect("join c1");
        let r2 = c2.await.expect("join c2");
        assert!(r1.is_err(), "stalled handshake must fail");
        assert!(r2.is_err(), "stalled handshake must fail");

        // Exactly two accepts total: the gate serialized them (one
        // after the other), it did not deduplicate failed dials.
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "serialized dials: one per caller, never concurrent"
        );
    }

    /// Panic-safety + no-double-dial-under-churn regression.
    ///
    /// 1. Panic-safety: if `ServerConn::connect` panics, the
    ///    `RemoveSlotOnDrop` RAII guard must still remove the gate slot
    ///    from `connecting` — otherwise the `Arc<Mutex<()>>` entry leaks
    ///    and every future caller for that addr serializes on a dead
    ///    gate. We can't make the real `connect` panic, so we drive the
    ///    gate-slot lifecycle directly: install a slot via the same
    ///    `entry().or_default()` path, then run a closure that panics
    ///    while a `RemoveSlotOnDrop` for that slot is live, and assert
    ///    the slot is gone afterwards.
    ///
    /// 2. No double-dial under churn: a late caller arriving after the
    ///    owning dialer removed its slot must observe the cached
    ///    connection on the re-check (or loop onto the current slot) —
    ///    never start a second concurrent dial. The serialization test
    ///    above already exercises the happy path; here we assert the
    ///    `connecting` map is empty once `get_or_connect` has returned,
    ///    i.e. the owning dialer's slot was removed (not leaked) after
    ///    publishing its failure.
    #[tokio::test]
    async fn gate_slot_removed_on_panic_and_after_dial() {
        use std::time::Duration;

        let pool = ConnectionPool::new();
        let addr: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // --- (1) panic-safety ---------------------------------------
        // Install a gate slot exactly as `get_or_connect` does.
        let gate = {
            let mut g = pool.connecting.lock();
            g.entry(addr).or_default().clone()
        };
        assert!(
            pool.connecting.lock().contains_key(&addr),
            "slot must be present before the (panicking) dial"
        );
        // Run a closure that panics while a `RemoveSlotOnDrop` is live.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot_guard = RemoveSlotOnDrop {
                pool: &pool,
                addr,
                gate: &gate,
            };
            panic!("simulated ServerConn::connect panic");
        }));
        assert!(panicked.is_err(), "closure must have panicked");
        assert!(
            !pool.connecting.lock().contains_key(&addr),
            "RAII guard must remove the gate slot even when the dial panics — \
             otherwise the slot leaks and future callers serialize on a dead gate"
        );

        // --- (2) no slot leak after a real (failed) dial ------------
        // A listener that accepts then stalls: the dial fails on
        // op_timeout. After `get_or_connect` returns, the `connecting`
        // map must be empty — the owning dialer removed its slot.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe listener");
        let probe_addr = listener.local_addr().expect("probe addr");
        let _srv = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // keep alive, never reply
            }
        });

        let res = pool
            .get_or_connect(
                probe_addr,
                "u",
                "h",
                Duration::from_millis(300),
                Duration::from_secs(40),
            )
            .await;
        assert!(res.is_err(), "stalled handshake must fail");
        assert!(
            !pool.connecting.lock().contains_key(&probe_addr),
            "the owning dialer must remove its gate slot after the dial \
             completes (success OR failure) — no churn-window leak"
        );
    }
}
