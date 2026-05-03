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

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::RwLock;
use tokio::sync::{Mutex, Notify};

use crate::error::{PvaError, PvaResult};

use super::beacon_throttle::BeaconTracker;
use super::search_engine::SearchEngine;
use super::server_conn::ServerConn;

static NEXT_CID: AtomicU32 = AtomicU32::new(1);

#[derive(Clone)]
pub enum ChannelState {
    Idle,
    Searching,
    Connecting,
    Active {
        server: Arc<ServerConn>,
        sid: u32,
        /// GUID expected for this server, captured from the
        /// SEARCH_RESPONSE that resolved the address. P-G12: on
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
    /// Shared connection pool (so multiple channels to the same server
    /// share a single TCP virtual circuit).
    pool: Arc<ConnectionPool>,
    resolver: Resolver,
    /// Alternative server addresses cached from the most recent search
    /// (excluding the one currently being tried). Multi-server failover:
    /// if `ensure_active` fails to connect or `CREATE_CHANNEL`s to the
    /// first server, it pops the next alternative before falling back to
    /// a fresh UDP search.
    alternatives: parking_lot::Mutex<Vec<std::net::SocketAddr>>,
    /// Earliest instant at which `ensure_active` is allowed to attempt a
    /// fresh connect. Set after a connect/CreateChannel failure to throttle
    /// reconnect storms — pvxs Channel::disconnect (client.cpp:155-163)
    /// pushes Connecting-stage failures 10 buckets (≈10 s) into the
    /// future. We accumulate exponentially per consecutive failure with a
    /// hard cap so a flapping server can't make every monitor caller spin.
    holdoff_until: parking_lot::Mutex<Option<std::time::Instant>>,
    /// Consecutive connect failures since the last successful Active
    /// transition. Used to scale `holdoff_until`.
    connect_fail_count: std::sync::atomic::AtomicU32,
    /// TCP `EPICS_PVA_NAME_SERVERS` fallbacks. Tried in order when UDP
    /// search yields no candidates. Empty for direct-mode and for
    /// channels created without name-server config.
    name_servers: Vec<std::net::SocketAddr>,
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
    /// replacement at the same address (P-G12).
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
    tls: parking_lot::Mutex<Option<Arc<crate::auth::TlsClientConfig>>>,
}

impl ConnectionPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enable TLS for every subsequent connect call.
    pub fn set_tls(&self, tls: Option<Arc<crate::auth::TlsClientConfig>>) {
        *self.tls.lock() = tls;
    }

    pub async fn get_or_connect(
        self: &Arc<Self>,
        addr: std::net::SocketAddr,
        user: &str,
        host: &str,
        op_timeout: std::time::Duration,
    ) -> PvaResult<Arc<ServerConn>> {
        // Fast path: existing alive conn.
        {
            let map = self.inner.lock();
            if let Some(conn) = map.get(&addr).cloned() {
                if conn.is_alive() {
                    return Ok(conn);
                }
            }
        }
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
        let fresh = match tls {
            Some(cfg) => {
                ServerConn::connect_tls(addr, &addr.ip().to_string(), cfg, user, host, op_timeout)
                    .await?
            }
            None => ServerConn::connect(addr, user, host, op_timeout).await?,
        };
        let mut map = self.inner.lock();
        // Race: someone else may have inserted. Prefer an alive existing one.
        if let Some(existing) = map.get(&addr).cloned() {
            if existing.is_alive() {
                return Ok(existing);
            }
        }
        map.insert(addr, fresh.clone());
        Ok(fresh)
    }

    pub fn close_dead(&self) {
        let mut map = self.inner.lock();
        map.retain(|_, conn| conn.is_alive());
    }

    /// Drop the cached connection for `addr` regardless of liveness
    /// (F10). Called when a GUID mismatch is detected at the same
    /// address — the previous code cleared its own Channel state but
    /// left the pool entry, so subsequent channels resolving to the
    /// same addr re-used the stale (wrong-GUID) ServerConn.
    pub fn invalidate(&self, addr: SocketAddr) {
        self.inner.lock().remove(&addr);
    }

    /// Drop every cached connection. The underlying `ServerConn`s live
    /// only as long as some `Arc` to them is held, so callers should
    /// already have dropped any operation handles that hold a clone.
    /// Used by `PvaClient::close` for explicit shutdown.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

impl Channel {
    pub fn new(
        pv_name: String,
        user: String,
        host: String,
        op_timeout: std::time::Duration,
        pool: Arc<ConnectionPool>,
        search: SearchEngine,
    ) -> Self {
        Self::new_with_name_servers(pv_name, user, host, op_timeout, pool, search, Vec::new())
    }

    /// Like [`Self::new`] but also accepts a TCP name-server fallback
    /// list. Pinged in order whenever UDP search yields no candidates.
    pub fn new_with_name_servers(
        pv_name: String,
        user: String,
        host: String,
        op_timeout: std::time::Duration,
        pool: Arc<ConnectionPool>,
        search: SearchEngine,
        name_servers: Vec<std::net::SocketAddr>,
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
            pool,
            resolver: Resolver::Search(search),
            alternatives: parking_lot::Mutex::new(Vec::new()),
            holdoff_until: parking_lot::Mutex::new(None),
            connect_fail_count: std::sync::atomic::AtomicU32::new(0),
            name_servers,
            server_destroyed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_destroyed_notify: Arc::new(Notify::new()),
            last_close_registration: parking_lot::Mutex::new(None),
            has_been_active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Construct a channel that targets a fixed server address (no UDP search).
    pub fn new_direct(
        pv_name: String,
        user: String,
        host: String,
        op_timeout: std::time::Duration,
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
            pool,
            resolver: Resolver::Direct(addr),
            alternatives: parking_lot::Mutex::new(Vec::new()),
            holdoff_until: parking_lot::Mutex::new(None),
            connect_fail_count: std::sync::atomic::AtomicU32::new(0),
            name_servers: Vec::new(),
            server_destroyed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_destroyed_notify: Arc::new(Notify::new()),
            last_close_registration: parking_lot::Mutex::new(None),
            has_been_active: std::sync::atomic::AtomicBool::new(false),
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
        // Quick happy-path check. P-G12: also verify that the
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
            // F10: also drop the stale pool entry so other channels
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
            tokio::time::sleep(d).await;
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

        // Pull a candidate server. Prefer cached alternatives from the
        // most recent multi-window search; otherwise issue a fresh search.
        // The lock guard from parking_lot is !Send, so we drop it before
        // any await.
        let cached: Option<Vec<std::net::SocketAddr>> = {
            let mut alts = self.alternatives.lock();
            if alts.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut *alts))
            }
        };
        let mut candidates = match cached {
            Some(list) => list,
            None => {
                self.set_state(ChannelState::Searching);
                // Pick `Reconnect` once we've ever been Active so the
                // search lands in the current bucket and waits for the
                // next 1 Hz tick (pvxs `Channel::disconnect` parity);
                // otherwise this is a fresh resolve and `Initial`
                // earns the immediate broadcast for fast first-attempt
                // latency.
                let reason = if self
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
                //   microseconds. We wrap in a short
                //   `MULTI_SERVER_WINDOW` (200 ms) so failure is
                //   surfaced quickly when no server answers (e.g.
                //   the user typed a wrong PV name).
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
                //   could fire (F6 dropped it as a zombie), and
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
                        let result = match reason {
                            super::search_engine::SearchReason::Initial => tokio::time::timeout(
                                super::search_engine::MULTI_SERVER_WINDOW,
                                engine.find(&self.pv_name, reason),
                            )
                            .await
                            .ok()
                            .and_then(|r| r.ok()),
                            super::search_engine::SearchReason::Reconnect => {
                                engine.find(&self.pv_name, reason).await.ok()
                            }
                        };
                        match result {
                            Some(addr) => vec![addr],
                            None => Vec::new(),
                        }
                    }
                    Resolver::Direct(addr) => vec![*addr],
                }
            }
        };

        // Append TCP name servers as final fallback candidates. pvxs
        // sends real SEARCH frames over a persistent TCP connection to
        // each name server (clientconn.cpp). For the common gateway-
        // self-serve case (gateway answers for any PV it proxies)
        // direct-connect to the name server's TCP port works
        // identically. Redirect-style chains aren't supported.
        if !self.name_servers.is_empty() {
            for ns in &self.name_servers {
                if !candidates.contains(ns) {
                    candidates.push(*ns);
                }
            }
        }

        if candidates.is_empty() {
            return Err(PvaError::Protocol("no servers found for PV".into()));
        }

        // Try each candidate in order; stash the rest as alternatives.
        let mut last_err: Option<PvaError> = None;
        for (idx, server_addr) in candidates.iter().enumerate() {
            self.set_state(ChannelState::Connecting);
            match self
                .pool
                .get_or_connect(*server_addr, &self.user, &self.host, self.op_timeout)
                .await
            {
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
                Ok(server) => match self.do_create_channel(&server).await {
                    Ok(sid) => {
                        // Stash remaining candidates as alternatives.
                        let leftovers: Vec<_> = candidates.iter().skip(idx + 1).copied().collect();
                        *self.alternatives.lock() = leftovers;
                        self.set_state(ChannelState::Active {
                            server: server.clone(),
                            sid,
                            // P-G12: capture the GUID our search
                            // engine recorded for this address. If a
                            // future reconnect to the same address
                            // observes a different beacon GUID, the
                            // ensure_active path can detect it.
                            expected_guid: self.resolver.last_guid_for(server.addr),
                        });
                        // Successful Active — clear holdoff state so the
                        // next eventual disconnect starts the backoff
                        // ladder from scratch instead of inheriting an
                        // old failure count.
                        self.connect_fail_count
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        *self.holdoff_until.lock() = None;
                        return Ok((server, sid));
                    }
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                },
            }
        }
        // Every candidate failed (search returned but TCP setup or
        // CREATE_CHANNEL bounced). Bump the consecutive-failure counter
        // and arm the holdoff window for the next call.
        let fails = self
            .connect_fail_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let secs = 1u64 << fails.saturating_sub(1).min(4); // 1, 2, 4, 8, 16
        *self.holdoff_until.lock() =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
        Err(last_err.unwrap_or_else(|| PvaError::Protocol("connect failed".into())))
    }

    fn set_state(&self, new_state: ChannelState) {
        // Tear down any previous SID-close registration (server-side
        // CMD_DESTROY_CHANNEL hook). Always do this on state change so a
        // stale `(sid, server)` entry can't fire spuriously after we've
        // moved past the old SID.
        let prev_reg = self.last_close_registration.lock().take();
        if let Some((old_sid, old_server)) = prev_reg {
            old_server.unregister_sid_close(old_sid);
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

        let big_endian = matches!(server.byte_order, crate::proto::ByteOrder::Big);
        let codec = PvaCodec { big_endian };
        let req = codec.build_create_channel(self.cid, &self.pv_name);

        // Register a one-shot waiter for the CREATE_CHANNEL response.
        let waiter = server.register_cid_waiter(self.cid);
        server.send(req).await?;

        let frame = tokio::time::timeout(self.op_timeout, waiter)
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
            pool,
            addr,
        )
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
}
