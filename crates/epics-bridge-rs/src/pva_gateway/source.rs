//! `ChannelSource` impl that bridges the gateway's [`ChannelCache`] to
//! the downstream [`epics_pva_rs::server`].
//!
//! Mirrors the role of `pva2pva GWServerChannelProvider` (server.cpp):
//! every downstream PVA op (search, get, put, monitor, get_field) is
//! resolved by looking up the PV name in the cache and forwarding to
//! the cached upstream channel. Monitor subscriptions are fanned out
//! through a per-entry tokio broadcast channel so multiple downstream
//! clients share one upstream subscription.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use epics_base_rs::server::access_security::AccessSecurityConfig;
// `AccessLevel` is referenced only by the `#[cfg(test)]` `acl_level`
// introspection helper, so the import is gated to match.
#[cfg(test)]
use epics_base_rs::server::access_security::AccessLevel;
use epics_pva_rs::client::{AssertedIdentity, PvaClient};
use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server::native_source::AcfCell;
use epics_pva_rs::server_native::source::{
    AccessChecked, ChannelContext, ChannelSource, OpError, WatermarkEvent, WatermarkKind,
};

use super::channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL};

/// Raw upstream MONITOR DATA body bytes flowing through the
/// per-entry broadcast channel. `body` is the wire-format
/// `changed | value | overrun` triplet refcount-shared via `Bytes`.
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub body: bytes::Bytes,
    pub byte_order: epics_pva_rs::proto::ByteOrder,
    /// when set, this event signals an upstream descriptor
    /// change. The body is meaningless under the downstream's
    /// original INIT descriptor — the downstream wire layer must
    /// emit `MONITOR FINISH` rather than forwarding the body bytes,
    /// because they were encoded for the new (incompatible)
    /// upstream descriptor.
    pub type_changed: bool,
}

/// one downstream→upstream pause/resume command queued to
/// the gateway's single watermark applier (spawned in
/// [`GatewayChannelSource::new`]).
///
/// * `cache` is the *single* cache layer the firing subscription's
///   upstream lives in — resolved from the downstream credential
///   context in the callback ([`GatewayChannelSource::upstream_cache_peek_for`]),
///   so one credential's backpressure never pauses another
///   credential's separate upstream for the same PV name (Finding 2).
/// * `op_id` identifies the downstream subscriber op casting this vote;
///   `seq` is the server's strictly-monotonic per-op watermark crossing
///   token. The applier folds `(op_id, seq, kind)` into the entry via
///   [`super::channel_cache::UpstreamEntry::apply_watermark_vote`], which
///   orders an op's own re-ordered transitions by `seq` (Finding 1) and
///   reference-counts pause votes across co-subscribers so the shared
///   upstream pauses only when every live op wants pause.
struct WmCommand {
    cache: Arc<ChannelCache>,
    name: String,
    op_id: u64,
    seq: u64,
    kind: WatermarkKind,
}

/// PV-name → ASG-name resolver. Returns the ASG that the gateway
/// should consult for the given downstream channel. Default impl
/// (see `default_asg_resolver`) returns `"DEFAULT"` for every name —
/// matching the legacy behaviour. Sites that want
/// per-PV granularity (e.g. `set:.*` → `OPERATOR`, `dev:.*` → `DEV`)
/// install a custom resolver via [`GatewayChannelSource::set_asg_resolver`].
///
/// MUST be cheap: this is called on every ACL check, which is on the
/// hot path of every GET / PUT / MONITOR. Avoid regex recompilation
/// or hashmap allocation per call — capture pre-built tables in the
/// closure.
pub type AsgResolver = Arc<dyn Fn(&str) -> String + Send + Sync>;

fn default_asg_resolver() -> AsgResolver {
    Arc::new(|_pv| "DEFAULT".to_string())
}

/// identity key for the per-credential upstream `PvaClient`
/// and `ChannelCache` pools.
///
/// Every field that the gateway forwards into the asserted upstream
/// identity must be part of this key. `upstream_client_for` builds the
/// upstream client from `account` + `host` and records `method` +
/// `authority` in the `AssertedIdentity`. Keying only on
/// `(account, method)` let two downstream peers with the same account
/// and method but different `host` / `authority` share the first
/// peer's upstream client and cache — the upstream IOC would then see
/// stale host / certificate-authority data for audit and ACF, and
/// cached GET/MONITOR state could be reused under the wrong identity.
///
/// Invariant: a pool entry is reused for a `ChannelContext` only when
/// every upstream-identity field matches. Adding a new identity field
/// to the asserted upstream credential MUST add it here too.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct UpstreamIdentityKey {
    account: String,
    method: String,
    host: String,
    authority: String,
}

impl UpstreamIdentityKey {
    fn from_ctx(ctx: &ChannelContext) -> Self {
        Self {
            account: ctx.account.clone(),
            method: ctx.method.clone(),
            host: ctx.host.clone(),
            authority: ctx.authority.clone(),
        }
    }
}

/// Capacity-bounded map with LRU eviction.
///
/// Bounds remote-controllable resource growth: a downstream client that
/// presents distinct identity keys on every connection can force at
/// most `max` upstream connections rather than an unlimited number.
/// When a new key would exceed the cap, the least-recently-used entry is
/// evicted first.
struct BoundedPool<K, V> {
    map: HashMap<K, (V, Instant)>,
    max: usize,
}

impl<K: Eq + Hash + Clone, V> BoundedPool<K, V> {
    fn new(max: usize) -> Self {
        Self {
            map: HashMap::new(),
            max: max.max(1),
        }
    }

    /// Current cap. Authoritative — there is no separate copy of this
    /// value elsewhere, so a diagnostic accessor that reads this can
    /// never desync from the pool's actual eviction bound.
    fn capacity(&self) -> usize {
        self.max
    }

    /// Snapshot every live value (cloned). Used by the gateway's
    /// all-layers cache administration so a control
    /// flush/drop/diagnostic can reach the per-credential caches
    /// without holding the pool lock across the async cache calls.
    fn values(&self) -> Vec<V>
    where
        V: Clone,
    {
        self.map.values().map(|(v, _)| v.clone()).collect()
    }

    /// Look up `key` and refresh its LRU timestamp on hit.
    fn get(&mut self, key: &K) -> Option<&V> {
        if let Some((v, t)) = self.map.get_mut(key) {
            *t = Instant::now();
            Some(v)
        } else {
            None
        }
    }

    /// Insert `key → value`. If the pool is at capacity and `key` is new,
    /// evict the least-recently-used entry first.
    fn insert(&mut self, key: K, value: V) {
        if !self.map.contains_key(&key) && self.map.len() >= self.max {
            if let Some(lru) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&lru);
            }
        }
        self.map.insert(key, (value, Instant::now()));
    }
}

/// `ChannelSource` impl handed to the downstream `PvaServer`. Cheap
/// to clone (Arc-backed cache + a couple of `Duration`s).
#[derive(Clone)]
pub struct GatewayChannelSource {
    cache: Arc<ChannelCache>,
    /// How long to wait for the upstream to deliver a first monitor
    /// event when a downstream client searches for a previously
    /// unseen PV. Pass through to `ChannelCache::lookup`.
    pub connect_timeout: Duration,
    /// Bridge subscriber-side mpsc capacity. Each downstream subscriber
    /// has its own bridge task that copies broadcast events into this
    /// mpsc; an overrun causes the next event to be dropped. Pick a
    /// generous default (matches pvxs queue size 64).
    pub subscriber_queue: usize,
    /// Per-call timeout for forwarded RPC requests. Configurable so
    /// long-running channelArchiver-style RPCs don't get cut off at
    /// an arbitrary 30 s ceiling. Default 30 s (matches pvxs).
    pub rpc_timeout: Duration,
    /// Hard cap on simultaneous live subscribe-bridge tasks across all
    /// downstream peers. The PvaServer enforces a per-connection
    /// channel cap; this is the gateway-wide ceiling that defends
    /// against a coordinated burst from many peers exhausting the
    /// gateway's monitor-fanout machinery. Default 100 000.
    pub max_subscribers: usize,
    /// Live subscribe-bridge counter (decremented when the bridge
    /// task exits). Shared via Arc so cloning the source preserves
    /// the count across the multiple `Arc<dyn ChannelSourceObj>`
    /// handles the runtime holds.
    subscriber_count: Arc<AtomicUsize>,
    /// Per-`UpstreamIdentityKey` upstream PvaClient pool.
    /// When the downstream peer authenticates as `(alice, ca)` from a
    /// given host/authority the gateway reuses (or builds) a client
    /// whose CONNECTION_VALIDATION to upstream advertises that same
    /// identity, so upstream ASG rules and audit logs see the *real*
    /// client identity, not the gateway. Anonymous / empty-account
    /// peers reuse the cache's shared client.
    ///
    /// keyed by account, method, host, AND authority — see
    /// [`UpstreamIdentityKey`].
    ///
    /// bounded via `BoundedPool` — evicts LRU entry when
    /// `max_upstream_identities` is reached, so a downstream client
    /// that presents unbounded distinct accounts cannot exhaust memory.
    upstream_pool: Arc<Mutex<BoundedPool<UpstreamIdentityKey, Arc<PvaClient>>>>,
    /// Optional gateway-side ACF policy. When set, every
    /// downstream GET / PUT / MONITOR is gated through
    /// `check_access_method` BEFORE the upstream forward, so the
    /// gateway can deny clients that the upstream IOC would also
    /// deny (or apply site-local policy on top of the upstream
    /// rules). Wrapped in an AcfCell so policy may be hot-swapped
    /// at runtime via `set_acf`. None means pass-through (legacy
    /// behaviour) — pvxs `pva2pva` parity for sites
    /// that delegate all ACL to upstream.
    acf: AcfCell,
    /// PV→ASG resolver. Defaults to `DEFAULT` for every name; sites
    /// that want per-PV ASG granularity replace this via
    /// [`set_asg_resolver`].
    ///
    /// Wrapped in `RwLock` so the resolver can
    /// be hot-swapped at runtime — the gateway is typically handed
    /// off to `PvaServer` behind an `Arc` (or its trait-object
    /// equivalent), so a `&mut self` setter is unreachable after
    /// installation. Mirrors the `acf` cell's hot-swap pattern.
    asg_resolver: Arc<RwLock<AsgResolver>>,
    /// Type-state-enforced access gate. The closure
    /// captures the `asg_resolver` cell so a hot-swap of the
    /// resolver via [`set_asg_resolver`] is visible on the next
    /// `check`. ASL is fixed at 0 for gateway-side checks — the
    /// upstream record's ASL is not visible to the gateway and the
    /// site policy on the gateway is expected to use UAG/HAG
    /// gating rather than per-record ASL.
    gate: epics_base_rs::server::access_security::AccessGate,
    /// per-`UpstreamIdentityKey` upstream ChannelCache pool.
    /// When a credentialed downstream peer issues a GET/MONITOR, the
    /// gateway routes through a cache backed by a per-credential
    /// PvaClient — so the upstream IOC sees the real downstream
    /// identity when evaluating its own ACF rules. Anonymous /
    /// empty-account peers fall through to the shared `cache`.
    /// Parallels `upstream_pool` which already provides per-credential
    /// routing for PUT / RPC / PROCESS.
    ///
    /// keyed by account, method, host, AND authority — see
    /// [`UpstreamIdentityKey`].
    ///
    /// bounded via `BoundedPool` (same cap as `upstream_pool`).
    upstream_caches: Arc<Mutex<BoundedPool<UpstreamIdentityKey, Arc<ChannelCache>>>>,
    /// feeds the single watermark pause/resume applier
    /// task spawned in [`Self::new`]. The sync `notify_watermark_*`
    /// callbacks push a [`WmCommand`] here with NO per-callback
    /// `tokio::spawn`, so there is exactly one totally-ordered owner of
    /// each upstream's pause state — the detached-spawn reorder that
    /// could otherwise leave an upstream wedged paused after a resume
    /// cannot happen. Cloning the source clones the sender (all clones
    /// feed the one applier); the applier exits when the last clone
    /// drops.
    wm_tx: mpsc::UnboundedSender<WmCommand>,
}

impl GatewayChannelSource {
    pub fn new(cache: Arc<ChannelCache>) -> Self {
        let acf: AcfCell = Arc::new(RwLock::new(None));
        let asg_resolver = Arc::new(RwLock::new(default_asg_resolver()));
        let gate = Self::build_gate(acf.clone(), asg_resolver.clone());
        // one ordered watermark applier per logical
        // source. `new` is the only constructor and `Clone` just copies
        // the sender, so exactly one applier task exists no matter how
        // many `Arc<dyn ChannelSourceObj>` handles the runtime holds.
        let (wm_tx, wm_rx) = mpsc::unbounded_channel::<WmCommand>();
        Self::spawn_watermark_applier(wm_rx);
        Self {
            cache,
            connect_timeout: Duration::from_secs(5),
            subscriber_queue: 64,
            rpc_timeout: Duration::from_secs(30),
            max_subscribers: 100_000,
            subscriber_count: Arc::new(AtomicUsize::new(0)),
            upstream_pool: Arc::new(Mutex::new(BoundedPool::new(256))),
            acf,
            asg_resolver,
            gate,
            upstream_caches: Arc::new(Mutex::new(BoundedPool::new(256))),
            wm_tx,
        }
    }

    fn build_gate(
        acf: AcfCell,
        asg_resolver: Arc<RwLock<AsgResolver>>,
    ) -> epics_base_rs::server::access_security::AccessGate {
        use epics_base_rs::server::access_security::{AccessGate, AsgAslResolver};
        let resolver: AsgAslResolver = Arc::new(move |pv_name| {
            let asg_resolver = asg_resolver.clone();
            Box::pin(async move {
                let g = asg_resolver.read().await;
                let asg = (g)(&pv_name);
                (asg, 0u8)
            })
        });
        AccessGate::required(acf, resolver)
    }

    /// Install (or hot-swap) the PV→ASG resolver. Mirrors the C IOC's
    /// per-record `ASG` field: each downstream channel name maps to
    /// the ASG that gates GET / PUT / MONITOR for that PV. The
    /// resolver is called on every op so should be O(1) on average
    /// — typically a pre-built `HashMap` or compiled-regex table.
    /// Pass `None` to reset to the `DEFAULT`-everywhere default.
    ///
    /// Takes `&self` and writes through `RwLock`, so the
    /// resolver may be replaced after the source has been handed to
    /// `PvaServer` behind an `Arc`.
    pub async fn set_asg_resolver(&self, resolver: Option<AsgResolver>) {
        *self.asg_resolver.write().await = resolver.unwrap_or_else(default_asg_resolver);
        // bump the gate's ACL generation so monitor tasks
        // observing this gate detect the policy swap on their next
        // event and re-check. Without this, gateway monitors after
        // a resolver hot-swap kept running under the prior ASG
        // mapping forever (the version compared the same value at
        // every event because the private counter on
        // `AccessGate::required()` never moved).
        self.gate.bump_acl_version();
    }

    /// Install (or hot-swap) the gateway-side ACF policy. None
    /// disables gateway-level enforcement and falls back to
    /// pass-through (upstream IOC remains the sole authority).
    pub async fn set_acf(&self, cfg: Option<AccessSecurityConfig>) {
        *self.acf.write().await = cfg;
        // bump the gate's ACL generation — see comment on
        // `set_asg_resolver`.
        self.gate.bump_acl_version();
    }

    // the `acf_cell()` accessor was removed. It
    // returned a clone of the inner `Arc<RwLock<Option<...>>>` so an
    // external coordinator (e.g. a multi-source PvaServer) could
    // hot-swap the policy by writing the cell directly — but that
    // path bypassed `gate.bump_acl_version()`, leaving monitor
    // tasks observing this gate at the pre-swap version forever.
    // Single-owner closure on the ACL-change transition: every
    // policy mutation MUST flow through `set_acf` or
    // `set_asg_resolver`, both of which bump the gate's version.
    // If a future requirement needs shared-AcfCell semantics across
    // multiple sources, the API will return a wrapper that wires
    // the version-bump into the write path.

    /// Test-only ACL introspection: evaluate the gateway-side ACL
    /// for `(pv, ctx)` and return the resolved [`AccessLevel`].
    /// Mirrors what the production `AccessGate::check` path would
    /// produce, but returns the bare level instead of an
    /// `AccessChecked` token so tests can inspect ACF behaviour
    /// without committing to a typed call.
    ///
    /// Gated on `#[cfg(test)]` so production
    /// code physically cannot bypass the gate by calling this
    /// alternate ACL path — the single owner of an ACL evaluation
    /// in a non-test build is `self.gate`.
    #[cfg(test)]
    async fn acl_level(&self, pv: &str, ctx: &ChannelContext) -> AccessLevel {
        let guard = self.acf.read().await;
        match *guard {
            None => AccessLevel::ReadWrite,
            Some(ref cfg) => {
                // read-lock the resolver cell; the closure
                // runs while the read lock is held so the swap is
                // serialized vs. in-flight evaluations. The closure
                // itself should be O(1) — comment on `AsgResolver`
                // documents this.
                let resolver = self.asg_resolver.read().await;
                let asg = (resolver)(pv);
                cfg.check_access_method(&asg, &ctx.host, &ctx.account, 0, &ctx.method, "")
            }
        }
    }

    /// Look up (or lazily build) the upstream client for the given
    /// downstream credentials. Each unique (account, method)
    /// pair gets its own connection so upstream ASG rules see the
    /// real client identity. Empty/anonymous credentials fall through
    /// to the cache's shared client (no new connection allocated).
    ///
    /// the pvAccess CONNECTION_VALIDATION handshake carries
    /// only the `ca` / `anonymous` auth methods, and the `ca`
    /// credential carries solely `user` + `host` (pvxs
    /// `clientconn.cpp:217-305` — `handle_CONNECTION_VALIDATION`
    /// selects only `"ca"` / `"anonymous"`; the `ca` cred sets only
    /// `cred["user"]` and `cred["host"]`). There is no wire method
    /// that forwards an `x509` downstream method or its certificate
    /// `AUTHORITY` upstream, so a PVA-to-PVA gateway *cannot* be
    /// transparent for non-`ca` methods — it converts the downstream
    /// identity into a CA-style assertion.
    ///
    /// To make that conversion explicit rather than silently
    /// indistinguishable from a first-party `ca` login, the upstream
    /// client is built with [`AssertedIdentity`] recording the real
    /// downstream `method` + certificate `authority`, and a non-`ca`
    /// downstream method is logged at `info` so the gateway's
    /// identity-assertion trust boundary is visible in audit output.
    fn upstream_client_for(&self, ctx: &ChannelContext) -> Arc<PvaClient> {
        if ctx.account.is_empty() || ctx.method == "anonymous" {
            return self.cache.client().clone();
        }
        let key = UpstreamIdentityKey::from_ctx(ctx);
        let mut pool = self.upstream_pool.lock();
        if let Some(c) = pool.get(&key) {
            return c.clone();
        }
        // a downstream method other than `ca` cannot be
        // forwarded verbatim — the upstream `ca` credential is a
        // gateway assertion. Make that explicit in audit output.
        if ctx.method != "ca" {
            tracing::info!(
                downstream_account = %ctx.account,
                downstream_method = %ctx.method,
                downstream_authority = %ctx.authority,
                downstream_host = %ctx.host,
                "PVA gateway asserting downstream identity upstream as CA-style \
                 credentials: the pvAccess wire cannot forward this auth method/authority",
            );
        }
        // Derive the per-credential client from the gateway's base
        // upstream client so it reaches the SAME upstream server /
        // transport — only the asserted identity differs. Building a
        // bare `PvaClient::builder()` here would drop the gateway's
        // `server_addr` and the client would fall back to UDP search,
        // never reaching a pinned or discovery-isolated upstream.
        let client = Arc::new(self.cache.client().with_asserted_identity(
            ctx.account.clone(),
            ctx.host.clone(),
            AssertedIdentity {
                downstream_method: ctx.method.clone(),
                downstream_authority: ctx.authority.clone(),
            },
        ));
        pool.insert(key, client.clone());
        client
    }

    /// look up (or lazily build) the upstream ChannelCache for
    /// `ctx`. Credentialed peers get a per-(account, method) cache backed
    /// by their own upstream PvaClient; anonymous peers reuse the shared
    /// `cache`. Parallels `upstream_client_for` which does the same for
    /// PUT/RPC/PROCESS.
    fn upstream_cache_for(&self, ctx: &ChannelContext) -> Arc<ChannelCache> {
        if ctx.account.is_empty() || ctx.method == "anonymous" {
            return self.cache.clone();
        }
        let key = UpstreamIdentityKey::from_ctx(ctx);
        // Fast path: already in pool.
        if let Some(c) = self.upstream_caches.lock().get(&key) {
            return c.clone();
        }
        // Build outside any lock — PvaClient::builder() is pure-Rust.
        let client = self.upstream_client_for(ctx);
        let new_cache = ChannelCache::with_max_entries(
            client,
            DEFAULT_CLEANUP_INTERVAL,
            // Per-credential ceiling: a single downstream identity is
            // unlikely to monitor more than this many PVs concurrently.
            // Bounded to prevent a single misbehaving peer from filling
            // the per-credential map indefinitely.
            1_024,
        );
        // Double-checked insert under lock: a racing caller may have won.
        let mut pool = self.upstream_caches.lock();
        if let Some(c) = pool.get(&key) {
            return c.clone();
        }
        pool.insert(key, new_cache.clone());
        new_cache
    }

    /// Cache handle — useful for the gateway's own diagnostics.
    pub fn cache(&self) -> &Arc<ChannelCache> {
        &self.cache
    }

    /// Update the upstream-identity pool cap on both `upstream_pool` and
    /// `upstream_caches`. Takes effect immediately; clears both pools so
    /// the next credential lookup builds fresh entries under the new cap.
    /// All clones of this `GatewayChannelSource` share the same pools and
    /// will see the new cap.
    pub fn set_max_upstream_identities(&self, n: usize) {
        let n = n.max(1);
        *self.upstream_pool.lock() = BoundedPool::new(n);
        *self.upstream_caches.lock() = BoundedPool::new(n);
    }

    /// current upstream-identity pool cap. Reads the live
    /// `upstream_pool` capacity directly, so this accessor can never
    /// desync from the cap actually enforced — unlike the removed
    /// `pub max_upstream_identities` field, which `set_max_upstream_identities`
    /// did not update. `upstream_caches` is always rebuilt with the
    /// same `n` by the setter, so the pool's cap is authoritative for
    /// both. Default 256.
    pub fn max_upstream_identities(&self) -> usize {
        self.upstream_pool.lock().capacity()
    }

    /// Diagnostic accessor: how many entries are currently cached.
    pub async fn cached_entry_count(&self) -> usize {
        self.cache.entry_count().await
    }

    /// flush the shared cache AND every per-credential
    /// upstream cache, returning the total number of entries removed.
    /// The control `<prefix>:flush` RPC routes through here so an
    /// operator flush actually tears down credentialed upstream
    /// monitors, not just the shared-identity ones. Caches are cloned
    /// out from under the pool lock first so the async `flush()` calls
    /// don't hold the (sync) `parking_lot::Mutex`.
    pub async fn flush_all_caches(&self) -> usize {
        let mut removed = self.cache.flush().await;
        let per_cred = self.upstream_caches.lock().values();
        for c in per_cred {
            removed += c.flush().await;
        }
        removed
    }

    /// drop one entry by name from the shared cache AND
    /// every per-credential upstream cache. Returns true if any layer
    /// held the entry. The control `<prefix>:drop` RPC routes through
    /// here so a credentialed upstream monitor for the named PV is also
    /// torn down.
    pub async fn drop_entry_all_caches(&self, name: &str) -> bool {
        let mut dropped = self.cache.drop_entry(name).await;
        let per_cred = self.upstream_caches.lock().values();
        for c in per_cred {
            dropped |= c.drop_entry(name).await;
        }
        dropped
    }

    /// total cached entries across the shared cache AND
    /// every per-credential upstream cache. The control `cacheSize` /
    /// `upstreamCount` diagnostics route through here so the operator
    /// does not see zero while credentialed upstream monitors remain
    /// alive.
    pub async fn total_cached_entry_count(&self) -> usize {
        let mut total = self.cache.entry_count().await;
        let per_cred = self.upstream_caches.lock().values();
        for c in per_cred {
            total += c.entry_count().await;
        }
        total
    }

    /// the single ordered owner of upstream pause/resume.
    /// Draining one mpsc with one task means every vote for a given entry
    /// is folded in one total order via
    /// [`super::channel_cache::UpstreamEntry::apply_watermark_vote`], which
    /// orders each op's own re-ordered transitions by `seq` (Finding 1)
    /// and reference-counts pause votes across co-subscribers — pausing
    /// the shared upstream only when every live downstream op wants pause
    /// and resuming as soon as one has room. `apply_..`
    /// returns `Some(_)` only on a real aggregate edge; on an edge the
    /// applier calls `reconcile_pause`, which re-reads the level and drives
    /// the installed `Pauser` through the entry's single serialized owner.
    /// That owner is shared with the reconnect loop's Pauser re-install, so
    /// a fresh upstream subscription installed mid-backpressure is paused
    /// immediately rather than running unthrottled until the next HIGH→LOW
    /// cycle. Because this is the sole writer of the entry's
    /// votes, the fold has no competing writer.
    fn spawn_watermark_applier(mut rx: mpsc::UnboundedReceiver<WmCommand>) {
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let Some(entry) = cmd.cache.peek(&cmd.name).await else {
                    continue;
                };
                if entry
                    .apply_watermark_vote(cmd.op_id, cmd.seq, cmd.kind)
                    .is_none()
                {
                    // No aggregate edge — the shared pause-state is
                    // unchanged by this vote, so the installed Pauser
                    // already matches and needs no drive.
                    continue;
                }
                // Drive on the edge, but through the single serialized
                // owner: `reconcile_pause` re-reads the level and is
                // mutually serialized with the reconnect loop's
                // re-install, so the installed Pauser always settles to
                // the current aggregate (no two-driver strand race).
                entry.reconcile_pause().await;
            }
        });
    }

    /// the cache layer the subscription identified by
    /// `ctx` monitors through, WITHOUT creating one. Anonymous /
    /// empty-account peers ride the shared `cache`; a credentialed peer
    /// rides its per-credential cache iff one already exists. Returns
    /// `None` for a credentialed peer with no live per-credential cache
    /// (nothing to pause). Mirrors the layer selection of
    /// [`Self::upstream_cache_for`] but is side-effect-free — a
    /// watermark crossing must not mint a new cache. Scoping the
    /// pause/resume to this one layer is what stops one credential's
    /// backpressure from throttling another credential's separate
    /// upstream for the same PV name (Finding 2).
    fn upstream_cache_peek_for(&self, ctx: &ChannelContext) -> Option<Arc<ChannelCache>> {
        if ctx.account.is_empty() || ctx.method == "anonymous" {
            return Some(self.cache.clone());
        }
        let key = UpstreamIdentityKey::from_ctx(ctx);
        self.upstream_caches.lock().get(&key).cloned()
    }

    /// resolve the firing subscription's single cache
    /// layer (per-credential targeting) and enqueue a pause vote to the
    /// ordered applier. Synchronous and non-blocking
    /// (`UnboundedSender::send`) — no per-callback spawn — so two
    /// near-simultaneous votes cannot be re-ordered by the runtime; their
    /// per-op `seq` (minted at the server's crossing) settles each op's
    /// state in the applier, and the applier reference-counts across ops.
    fn queue_watermark(&self, name: &str, ev: &WatermarkEvent, ctx: &ChannelContext) {
        let Some(cache) = self.upstream_cache_peek_for(ctx) else {
            // Credentialed peer with no live upstream cache — nothing to
            // pause/resume.
            return;
        };
        let _ = self.wm_tx.send(WmCommand {
            cache,
            name: name.to_string(),
            op_id: ev.op_id,
            seq: ev.seq,
            kind: ev.kind,
        });
    }

    /// Diagnostic: live subscribe-bridge tasks.
    pub fn live_subscribers(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Test accessor: returns the upstream cache that would be
    /// selected for `ctx`. Exposed so tests can verify per-credential
    /// cache separation without a live upstream IOC.
    #[cfg(test)]
    fn upstream_cache_for_test(&self, ctx: &ChannelContext) -> Arc<ChannelCache> {
        self.upstream_cache_for(ctx)
    }

    /// Internal raw-subscribe helper. Routes `subscribe_raw` and
    /// `subscribe_raw_checked` through a caller-supplied cache so
    /// credentialed peers get per-credential upstream entries.
    async fn subscribe_raw_inner(
        &self,
        cache: Arc<ChannelCache>,
        name: &str,
    ) -> Option<mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>> {
        // default ON — opt out via EPICS_PVA_GW_RAW_FRAMES=NO.
        if let Some(v) = epics_base_rs::runtime::env::get("EPICS_PVA_GW_RAW_FRAMES") {
            if v.eq_ignore_ascii_case("NO") || v.eq_ignore_ascii_case("FALSE") || v == "0" {
                return None;
            }
        }
        // bump counter before spawning forwarder.
        let prev = self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        if prev >= self.max_subscribers {
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
            tracing::warn!(
                pv = %name,
                live = prev,
                cap = self.max_subscribers,
                "pva-gateway: raw subscriber cap reached, refusing"
            );
            return None;
        }
        let entry = match cache.lookup(name, self.connect_timeout).await {
            Ok(e) => e,
            Err(_) => {
                self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        };
        // subscribe before snapshot to avoid losing events in the
        // gap, but the same gap can deliver event E to both the broadcast
        // receiver (created here) and snapshot_raw() (read below).
        // Dedup the first broadcast event by Bytes ptr when it matches.
        let mut bcast = entry.subscribe_raw();
        // deliver cached initial snapshot to new raw subscriber.
        // pva2pva moncache.cpp:285-311 delivers `lastelem` on start();
        // the typed path (subscribe_inner) does the same via snapshot().
        let initial_raw = entry.snapshot_raw();
        // capture the Bytes pointer of the initial snapshot so the
        // broadcast loop can skip the duplicate if the same event landed in
        // both the snapshot cache and the broadcast queue (the race window
        // between subscribe_raw() and snapshot_raw() above).
        // bytes::Bytes::clone() is a refcount clone; same allocation ⟹ same
        // as_ptr(). Take the Option once so only a single event is skipped.
        let dedup_body: Option<bytes::Bytes> = initial_raw.as_ref().map(|e| e.body.clone());
        let (mpsc_tx, mpsc_rx) =
            mpsc::channel::<epics_pva_rs::server_native::RawMonitorEvent>(self.subscriber_queue);
        let counter = self.subscriber_count.clone();
        tokio::spawn(async move {
            struct CounterGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for CounterGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let _guard = CounterGuard(counter);
            if let Some(init) = initial_raw {
                let out = epics_pva_rs::server_native::RawMonitorEvent {
                    body_bytes: init.body,
                    byte_order: init.byte_order,
                    type_changed: false,
                };
                if mpsc_tx.send(out).await.is_err() {
                    return;
                }
            }
            let mut dedup = dedup_body;
            loop {
                match bcast.recv().await {
                    Ok(ev) => {
                        if let Some(d) = dedup.take() {
                            if ev.body.as_ptr() == d.as_ptr() {
                                continue;
                            }
                        }
                        let type_changed = ev.type_changed;
                        let out = epics_pva_rs::server_native::RawMonitorEvent {
                            body_bytes: ev.body,
                            byte_order: ev.byte_order,
                            type_changed,
                        };
                        if mpsc_tx.send(out).await.is_err() {
                            return;
                        }
                        // type-change marker is end-of-stream.
                        if type_changed {
                            return;
                        }
                    }
                    // dropped events under this receiver's own
                    // backpressure are skipped with NO overrun signal to the
                    // client. pva2pva coalesces into an overflowElement and
                    // sets the overrun bitset (moncache.cpp:157-174). Faithful
                    // signaling needs the epics-pva-rs monitor encoder — see
                    // legend (cross-crate, deferred).
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Some(mpsc_rx)
    }

    /// Internal typed-subscribe helper. Routes `subscribe` and
    /// `subscribe_checked` through a caller-supplied cache.
    async fn subscribe_inner(
        &self,
        cache: Arc<ChannelCache>,
        name: &str,
    ) -> Option<mpsc::Receiver<PvField>> {
        // Gateway-wide subscriber cap.
        let prev = self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        if prev >= self.max_subscribers {
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
            tracing::warn!(
                pv = %name,
                live = prev,
                cap = self.max_subscribers,
                "pva-gateway: subscriber cap reached, refusing"
            );
            return None;
        }
        let entry = match cache.lookup(name, self.connect_timeout).await {
            Ok(e) => e,
            Err(_) => {
                self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut bcast_rx = entry.subscribe();
        let initial = entry.snapshot();
        let (mpsc_tx, mpsc_rx) = mpsc::channel(self.subscriber_queue);
        let counter = self.subscriber_count.clone();
        tokio::spawn(async move {
            struct CounterGuard(Arc<AtomicUsize>);
            impl Drop for CounterGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Relaxed);
                }
            }
            let _guard = CounterGuard(counter);
            if let Some(v) = initial {
                if mpsc_tx.send(v).await.is_err() {
                    return;
                }
            }
            loop {
                match bcast_rx.recv().await {
                    Ok(v) => {
                        if mpsc_tx.send(v).await.is_err() {
                            return;
                        }
                    }
                    // same as the raw path — drops under this
                    // receiver's backpressure carry no overrun signal
                    // (pva2pva moncache.cpp:157-174). Cross-crate, deferred.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Some(mpsc_rx)
    }
}

impl ChannelSource for GatewayChannelSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        &self.gate
    }

    async fn list_pvs(&self) -> Vec<String> {
        self.cache.names().await
    }

    async fn has_pv(&self, name: &str) -> bool {
        // Trigger an upstream lookup so the very first downstream
        // SEARCH for a previously-unseen PV resolves correctly.
        // Subsequent calls hit the fast path.
        self.cache.lookup(name, self.connect_timeout).await.is_ok()
    }

    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        let entry = self.cache.lookup(name, self.connect_timeout).await.ok()?;
        entry.introspection()
    }

    /// a credentialed downstream CREATE_CHANNEL resolves
    /// existence against THAT peer's per-credential upstream cache, not
    /// the shared gateway cache, so channel setup never opens or refreshes
    /// upstream state under the shared identity. Anonymous peers fall back
    /// to the shared cache (`upstream_cache_for` returns `self.cache`), so
    /// their behaviour is unchanged. The credential-free `has_pv` is kept
    /// for the connectionless UDP SEARCH path, which carries no identity.
    async fn has_pv_checked(&self, name: &str, ctx: ChannelContext) -> bool {
        self.upstream_cache_for(&ctx)
            .lookup(name, self.connect_timeout)
            .await
            .is_ok()
    }

    /// descriptor discovery for a credentialed downstream
    /// peer (CREATE_CHANNEL GET-INIT / GET_FIELD) reads the upstream type
    /// through that peer's per-credential cache — the same cache
    /// `get_value_checked`/`subscribe_checked` use — so the upstream audit
    /// identity matches the operation that follows.
    async fn get_introspection_checked(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Option<FieldDesc> {
        let entry = self
            .upstream_cache_for(&ctx)
            .lookup(name, self.connect_timeout)
            .await
            .ok()?;
        entry.introspection()
    }

    async fn get_value(&self, name: &str) -> Option<PvField> {
        let entry = self.cache.lookup(name, self.connect_timeout).await.ok()?;
        // Prefer the cached monitor snapshot — same value the upstream
        // server would return to a fresh GET, no extra round-trip.
        entry.snapshot()
    }

    /// Ctx-less PUT (no downstream credentials). BUG 3: this path
    /// MUST apply the same gateway-side ACF gate as
    /// `put_value_checked` — pre-fix it forwarded `pvput`
    /// unconditionally, so a `read_only` / deny-WRITE ACF policy was
    /// silently inert for any non-credentialed PUT. The check runs
    /// with empty/anonymous credentials (the only identity available
    /// on this path); when no ACF is installed the gate returns
    /// `ReadWrite`, preserving the legacy pass-through.
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), OpError> {
        let checked = self.gate.check(name, "", "", "anonymous", "").await;
        if !checked.allows_write() {
            tracing::debug!(
                pv = %name,
                "pva-gateway: ctx-less PUT denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PUT denied by gateway access security: PV '{name}' (anonymous)"
            )));
        }
        // Look up the entry to keep the upstream channel alive (and
        // confirm the PV exists) before issuing the PUT through the
        // shared client. The client's connection pool reuses the
        // already-open server connection.
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| OpError::failed(e.to_string()))?;
        // typed pass-through — forward the PvField as-is without
        // re-encoding through string form. pvxs serialises the PUT value
        // with to_wire_valid(R, temp) (pvxs/src/clientget.cpp:305) — no
        // string round-trip in the reference implementation.
        self.cache
            .client()
            .pvput_pv_field(name, &value)
            .await
            .map_err(|e| OpError::failed(e.to_string()))
    }

    /// Credential-aware PUT — migration to the
    /// type-state API. Routes the put through a per-(account,
    /// method) upstream PvaClient so the upstream IOC's ASG rules
    /// see the real downstream identity instead of the gateway's.
    /// Anonymous / empty-account peers fall back to the shared
    /// client. The AccessChecked token gates entry — `NoAccess` or
    /// `Read`-only peers receive the same gateway-identifying error
    /// the default put_value_checked would produce.
    async fn put_value_checked(
        &self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                method = %ctx.method,
                "pva-gateway: PUT denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PUT denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.host,
                account = ctx.account,
                method = ctx.method,
            )));
        }

        let name = checked.pv_name();
        // no shared-cache preflight. The earlier
        // `self.cache.lookup(...)` confirmed existence but warmed the
        // *shared* client's pool and opened an upstream monitor under
        // the gateway/shared identity — yet the PUT below runs through
        // the per-credential `upstream_client_for(&ctx)` client, so the
        // preflight warmed the wrong pool and leaked a shared-identity
        // monitor as a side effect of a credentialed write. Let the
        // per-credential client resolve existence and surface the
        // upstream error itself, exactly as GET/MONITOR do.
        let client = self.upstream_client_for(&ctx);
        tracing::debug!(
            pv = %name,
            account = %ctx.account,
            method = %ctx.method,
            "pva-gateway: forwarding PUT with downstream credentials"
        );
        // typed pass-through (see put_value for rationale).
        client
            .pvput_pv_field(name, &value)
            .await
            .map_err(|e| OpError::failed(e.to_string()))
    }

    async fn is_writable(&self, name: &str) -> bool {
        // Peek-only: report writable iff the entry is already in the
        // cache. We deliberately do NOT trigger a fresh upstream
        // lookup here. If we did, a malicious or buggy client could
        // probe N random names against `is_writable` and force N
        // upstream search-and-subscribe cycles, each holding an
        // upstream-monitor task open until `connect_timeout` fires
        // (search-storm vector). The honest answer for an unseen PV
        // is "I don't know yet" — pvxs convention treats that as
        // not-writable, which is what we return.
        self.cache.peek(name).await.is_some()
    }

    /// Forward an RPC request through the upstream client. The default
    /// trait impl returns "RPC not supported", which is a major p2pApp
    /// parity gap (review §1). With this override, RPC requests pass
    /// through transparently — `pvrpc` reuses the cached channel
    /// connection-pool entry so we don't pay a fresh search per call.
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<(FieldDesc, PvField), OpError> {
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| OpError::failed(e.to_string()))?;
        let result = tokio::time::timeout(
            self.rpc_timeout,
            self.cache
                .client()
                .pvrpc(name, &request_desc, &request_value),
        )
        .await;
        match result {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(OpError::failed(e.to_string())),
            Err(_) => Err(OpError::failed(format!("upstream rpc timeout for {name}"))),
        }
    }

    /// Credential-aware RPC. RPC is treated as WRITE-class for ACF: a
    /// peer without write access is refused at the gateway with a
    /// gateway-identifying error (so an unauthorised caller cannot
    /// trigger archiver-control / state-change RPCs on the upstream
    /// IOC). pva2pva classifies `createChannelRPC` write-class — it
    /// blocks RPC under `p2pReadOnly` alongside Put/Process
    /// (`channel.cpp:140-150`) — and pvxs `sharedpv.cpp:162-179` has no
    /// built-in read-class RPC gate, so the prior READ-class gating let
    /// a read-only peer drive state-changing RPCs. On allow, the request
    /// is forwarded through the per-(account, method) upstream client
    /// pool — the same identity-preserving routing `put_value_checked`
    /// uses — so the upstream IOC's ASG rules and audit logs see the
    /// real downstream identity instead of the gateway's.
    async fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<(FieldDesc, PvField), OpError> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                method = %ctx.method,
                "pva-gateway: RPC denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "RPC denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.host,
                account = ctx.account,
                method = ctx.method,
            )));
        }
        let name = checked.pv_name();
        // no shared-cache preflight (see put_value_checked).
        // The RPC runs through the per-credential client, which resolves
        // existence and returns the upstream error itself.
        let client = self.upstream_client_for(&ctx);
        let result = tokio::time::timeout(
            self.rpc_timeout,
            client.pvrpc(name, &request_desc, &request_value),
        )
        .await;
        match result {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(OpError::failed(e.to_string())),
            Err(_) => Err(OpError::failed(format!("upstream rpc timeout for {name}"))),
        }
    }

    /// Forward a PVA PROCESS (wire cmd 16) to the upstream PV. The
    /// `ChannelSource` default returns `Ok(())` — for a proxying
    /// gateway that silently swallows the PROCESS and falsely reports
    /// success, so a downstream `caput -c` / `pvcall .PROC` never
    /// actually triggers the upstream record. This override resolves
    /// the PV through the cache (confirming it exists) and forwards
    /// `pvprocess` through the shared client.
    async fn process(&self, name: &str) -> Result<(), OpError> {
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| OpError::failed(e.to_string()))?;
        self.cache
            .client()
            .pvprocess(name)
            .await
            .map_err(|e| OpError::failed(e.to_string()))
    }

    /// Credential-aware PROCESS. pvxs treats PROCESS as a WRITE-class
    /// operation for ACF (it mutates record state), so a non-write
    /// token is refused with a gateway-identifying error. On allow
    /// the request is forwarded through the per-(account, method)
    /// upstream client pool — the same identity-preserving routing
    /// `put_value_checked` uses.
    async fn process_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                method = %ctx.method,
                "pva-gateway: PROCESS denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PROCESS denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.host,
                account = ctx.account,
                method = ctx.method,
            )));
        }
        let name = checked.pv_name();
        // no shared-cache preflight (see put_value_checked).
        // PROCESS runs through the per-credential client, which resolves
        // existence and returns the upstream error itself.
        let client = self.upstream_client_for(&ctx);
        client
            .pvprocess(name)
            .await
            .map_err(|e| OpError::failed(e.to_string()))
    }

    async fn subscribe_raw(
        &self,
        name: &str,
    ) -> Option<mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>> {
        self.subscribe_raw_inner(self.cache.clone(), name).await
    }

    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
        self.subscribe_inner(self.cache.clone(), name).await
    }

    /// route GET through per-credential upstream cache so the
    /// upstream IOC sees the real downstream identity. Pre-fix the
    /// default trait impl called `self.get_value(name)` which used the
    /// shared cache regardless of downstream credentials.
    async fn get_value_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        if !checked.allows_read() {
            return None;
        }
        let cache = self.upstream_cache_for(&ctx);
        let entry = cache
            .lookup(checked.pv_name(), self.connect_timeout)
            .await
            .ok()?;
        entry.snapshot()
    }

    /// route MONITOR through per-credential upstream cache.
    async fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<PvField>> {
        if !checked.allows_read() {
            return None;
        }
        self.subscribe_inner(self.upstream_cache_for(&ctx), checked.pv_name())
            .await
    }

    /// route raw MONITOR through per-credential upstream cache.
    async fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>> {
        if !checked.allows_read() {
            return None;
        }
        self.subscribe_raw_inner(self.upstream_cache_for(&ctx), checked.pv_name())
            .await
    }

    /// decoded MONITOR with the downstream's event-affecting
    /// pvRequest options.
    ///
    /// The gateway fans **one** upstream monitor — opened with the
    /// gateway's default pvRequest — out to every downstream
    /// subscriber for a PV name (`channel_cache::spawn_upstream_monitor`
    /// keys the cache by PV name only). Field projection, `pipeline`,
    /// and `queueSize` are downstream-local — the gateway terminates
    /// them on its own downstream connection / outbox — and are
    /// transparent. A server-side `_filter` chain (pvxs
    /// `servermon.cpp:521-555`), by contrast, changes *upstream event
    /// production* and cannot be honored across the shared upstream
    /// monitor: serving such a subscription from the default fanout
    /// would deliver an event stream that differs from a direct
    /// upstream monitor.
    ///
    /// This source is a documented cache/fanout gateway, so the
    /// parity-correct behaviour is to **reject** an unsupported
    /// option set (return `None`) rather than silently serve diverging
    /// events. A subscription with no event-affecting option delegates
    /// to the normal ACF-gated `subscribe_checked` path.
    async fn subscribe_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<mpsc::Receiver<PvField>> {
        if opts.affects_upstream_events() {
            tracing::warn!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                pipeline = opts.pipeline,
                queue_size = ?opts.queue_size,
                server_filter = opts.server_filter,
                "pva-gateway: rejecting MONITOR — event-affecting pvRequest \
                 options cannot be honored transparently across the gateway's \
                 single fanout upstream monitor",
            );
            return None;
        }
        // A subscription with no event-affecting option delegates to the
        // normal ACF-gated `subscribe_checked` path. The gateway fans out
        // plain upstream values and carries no QSRV `+trigger` graph; the
        // cooked `subscribe_checked_opts_marked` variant the server
        // dispatches on inherits this rejection via the trait default
        // (which routes through this method) and wraps each value with
        // `marked: None`, like every non-group source.
        self.subscribe_checked(checked, ctx).await
    }

    /// Raw-path counterpart of [`Self::subscribe_checked_opts`].
    /// Same reject-on-unsupported-option contract.
    async fn subscribe_raw_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>> {
        if opts.affects_upstream_events() {
            tracing::warn!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                pipeline = opts.pipeline,
                queue_size = ?opts.queue_size,
                server_filter = opts.server_filter,
                "pva-gateway: rejecting raw MONITOR — event-affecting pvRequest \
                 options cannot be honored transparently across the gateway's \
                 single fanout upstream monitor",
            );
            return None;
        }
        self.subscribe_raw_checked(checked, ctx).await
    }

    /// expose pipeline-window watermark levels so the
    /// server's monitor loop drives pause/resume on the feeding upstream
    /// monitor. Pre-FR-11 `GatewayChannelSource` did NOT override this,
    /// so the trait default `None` made the pause/resume callbacks
    /// unreachable — the `Pauser` plumbing existed but nothing could fire
    /// it.
    ///
    /// `(low, high) = (0, 0)`: pause the upstream when the downstream
    /// pipeline window is fully drained (0 credit — the gateway cannot
    /// emit anyway) and resume as soon as a client ACK returns any
    /// credit. The `(0, 0)` choice is independent of the client-
    /// negotiated queue size and therefore deadlock-free for any window:
    /// `high == 0` guarantees the ACK path can always re-cross it
    /// (`w_now > 0`) and resume a paused upstream. (A non-zero `high`
    /// could exceed a small client window and never re-fire after a
    /// pause.) Name-independent: the gateway applies the same `(0, 0)`
    /// pipeline-window policy to every monitor it serves. The composite
    /// resolves the owning source via `has_pv` before reading levels, so
    /// this is consulted only for names the gateway actually owns —
    /// returning `Some` for every name no longer shadows a co-registered
    /// name-scoped source's real per-PV levels.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        let _ = name;
        Some((0, 0))
    }

    /// Forward downstream pipeline-window flow control into upstream
    /// pause/resume. pvxs pipeline-window semantics
    /// (`servermon.cpp`/`source.h`): `Resume` means a client ACK refilled
    /// the window above the high mark — the downstream is keeping up, so
    /// its pause vote is dropped; `Pause` means a DATA emission drained the
    /// window to/below low — the client is falling behind, so it votes to
    /// pause the feeding upstream. `Withdraw` (the op's subscriber task
    /// ended) removes its vote so a torn-down op cannot strand the shared
    /// upstream paused for its co-subscribers. The per-PV `Pauser` is
    /// installed by the auto-restart task in
    /// `channel_cache.rs::spawn_upstream_monitor`.
    ///
    /// The vote is resolved to the *single* cache
    /// layer this credential monitors through (per-credential targeting)
    /// and queued to the single ordered applier with the
    /// server's per-op crossing `seq` (deadlock-free ordering)
    /// and `op_id` (cross-op reference counting). Best
    /// effort: if the entry isn't currently connected upstream the
    /// pause/resume is a no-op in the applier.
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        match ev.kind {
            WatermarkKind::Pause => tracing::warn!(
                pv = %name, op_id = ev.op_id, seq = ev.seq,
                "pva-gateway: downstream pipeline window drained below low — voting pause"
            ),
            WatermarkKind::Resume => tracing::debug!(
                pv = %name, op_id = ev.op_id, seq = ev.seq,
                "pva-gateway: downstream pipeline window refilled above high — releasing pause vote"
            ),
            WatermarkKind::Withdraw => tracing::debug!(
                pv = %name, op_id = ev.op_id,
                "pva-gateway: downstream monitor op ended — withdrawing pause vote"
            ),
        }
        self.queue_watermark(name, &ev, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::access_security::parse_acf;
    use epics_pva_rs::server_native::source::OpErrorKind;

    fn make_ctx(host: &str, account: &str, method: &str) -> ChannelContext {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            account: account.to_string(),
            method: method.to_string(),
            host: host.to_string(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
        }
    }

    fn make_source() -> GatewayChannelSource {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(client, Duration::from_secs(60));
        GatewayChannelSource::new(cache)
    }

    /// Mint an [`AccessChecked`] token for `(pv, ctx)` through the
    /// source's own gate — the same path tcp.rs uses before calling
    /// the typed `_checked` op methods. Used by the tests below to
    /// exercise the type-state API after the `_ctx` methods were
    /// folded into the `_checked` family.
    async fn check(src: &GatewayChannelSource, pv: &str, ctx: &ChannelContext) -> AccessChecked {
        src.access()
            .check(pv, &ctx.host, &ctx.account, &ctx.method, "")
            .await
    }

    /// operator cache administration must span the shared
    /// cache AND every per-credential upstream cache, not just the
    /// shared one. An entry living only in a credentialed cache must be
    /// counted, droppable, and flushable through the gateway-level
    /// all-layers API the control RPCs route through.
    #[tokio::test]
    async fn control_admin_spans_per_credential_caches() {
        let src = make_source();
        // One entry in the shared cache (anonymous identity).
        src.cache.insert_test_entry("SHARED:PV").await;
        // One entry in a per-credential cache (alice/ca) — built via the
        // same selector the credentialed GET/MONITOR path uses.
        let ctx = make_ctx("h", "alice", "ca");
        let cred_cache = src.upstream_cache_for_test(&ctx);
        cred_cache.insert_test_entry("CRED:PV").await;

        // Count aggregates both layers (shared-only would report 1).
        assert_eq!(src.total_cached_entry_count().await, 2);
        // Drop reaches the per-credential entry (shared-only drop_entry
        // would return false and leave the credentialed monitor alive).
        assert!(src.drop_entry_all_caches("CRED:PV").await);
        assert_eq!(src.total_cached_entry_count().await, 1);
        // Flush clears the remaining shared entry too.
        assert_eq!(src.flush_all_caches().await, 1);
        assert_eq!(src.total_cached_entry_count().await, 0);
    }

    /// the gateway source MUST expose pipeline-window
    /// watermark levels so the server's monitor loop drives upstream
    /// pause/resume. Pre-FR-11 it inherited the trait default `None`,
    /// leaving the `Pauser` callbacks unreachable. `(0, 0)` is the
    /// deadlock-free choice (HIGH always re-crosses on the first ACK
    /// credit regardless of the client's negotiated window).
    #[tokio::test]
    async fn fr11_gateway_exposes_watermark_levels() {
        let src = make_source();
        assert_eq!(
            <GatewayChannelSource as ChannelSource>::monitor_watermarks(&src, "ANY:PV").await,
            Some((0, 0)),
            "gateway must expose watermark levels so pause/resume is reachable"
        );
    }

    /// A watermark crossing must
    /// pause/resume ONLY the cache layer the firing credential monitors
    /// through. `upstream_cache_peek_for` resolves that single layer
    /// (and never creates one), so one credential's backpressure cannot
    /// reach another credential's separate upstream for the same PV
    /// name — the over-throttle the pre-review all-layers walk caused.
    #[tokio::test]
    async fn fr11_watermark_targets_only_the_firing_credentials_cache() {
        let src = make_source();

        // Anonymous peers ride the shared cache.
        let anon = make_ctx("h", "", "anonymous");
        let shared = src
            .upstream_cache_peek_for(&anon)
            .expect("anonymous always resolves to the shared cache");
        assert!(
            Arc::ptr_eq(&shared, &src.cache),
            "anonymous peers ride the shared cache"
        );

        // A credentialed peer with no live per-credential cache resolves
        // to None — a watermark must not mint a cache (side-effect-free).
        let alice = make_ctx("h", "alice", "ca");
        assert!(
            src.upstream_cache_peek_for(&alice).is_none(),
            "no live per-credential cache yet → nothing to pause"
        );

        // Once alice's and bob's per-credential caches exist, alice's
        // crossing resolves to alice's OWN cache — never the shared
        // cache and never bob's.
        let alice_cache = src.upstream_cache_for_test(&alice);
        let bob = make_ctx("h", "bob", "ca");
        let bob_cache = src.upstream_cache_for_test(&bob);

        let alice_resolved = src
            .upstream_cache_peek_for(&alice)
            .expect("alice's cache now exists");
        assert!(
            Arc::ptr_eq(&alice_resolved, &alice_cache),
            "alice's backpressure targets alice's own upstream cache"
        );
        assert!(
            !Arc::ptr_eq(&alice_resolved, &bob_cache),
            "alice's backpressure must NOT reach bob's separate upstream"
        );
        assert!(
            !Arc::ptr_eq(&alice_resolved, &src.cache),
            "a credentialed peer's upstream is not the shared cache"
        );
    }

    /// Baseline: with no ACF attached, `acl_level` reports
    /// `ReadWrite` so the gateway's pre-existing pass-through fast
    /// path stays hot. This was previously the only behaviour.
    #[tokio::test]
    async fn acl_level_no_acf_is_readwrite() {
        let src = make_source();
        let level = src
            .acl_level("any:pv", &make_ctx("h", "anyone", "anonymous"))
            .await;
        assert!(matches!(level, AccessLevel::ReadWrite));
    }

    /// With an ACF attached, downstream peers must match
    /// the DEFAULT ASG's rules. PUT is denied for callers not in
    /// the WRITE rule; GET still goes through when READ is granted
    /// (none of these tests hit `cache.lookup` because the ACL
    /// check runs first and returns Err/None).
    #[tokio::test]
    async fn put_value_ctx_denied_when_acf_no_write() {
        let src = make_source();
        let cfg = parse_acf(
            r#"
UAG(admins) { admin }
ASG(DEFAULT) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        // PUT as non-admin → must be denied at the gateway (no
        // upstream lookup needed).
        let dummy_value = PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0));
        let ctx = make_ctx("h", "intruder", "anonymous");
        let token = check(&src, "any:pv", &ctx).await;
        let err = src
            .put_value_checked(token, dummy_value, ctx)
            .await
            .expect_err("PUT must be denied for non-admin under DEFAULT ASG");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "gateway ACF write denial must classify as Denied: {err:?}"
        );
        assert!(
            err.message.contains("denied by gateway access security"),
            "denial reason must name the gateway as enforcement point: {err:?}",
        );
    }

    /// A NoAccess rule denies GET and MONITOR (returns
    /// None — same shape as unknown PV at the wire layer).
    #[tokio::test]
    async fn get_and_subscribe_denied_when_acf_no_access() {
        let src = make_source();
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(1, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("h", "intruder", "anonymous");
        let get_token = check(&src, "any:pv", &ctx).await;
        assert!(
            src.get_value_checked(get_token, ctx.clone())
                .await
                .is_none(),
            "GET must be denied for non-ops"
        );
        let sub_token = check(&src, "any:pv", &ctx).await;
        assert!(
            src.subscribe_checked(sub_token, ctx).await.is_none(),
            "MONITOR must be denied for non-ops"
        );
    }

    /// Hot-swapping the ACF cell takes effect on the next
    /// op. Mirrors the PVA-server-side AcfCell test.
    #[tokio::test]
    async fn acf_swap_takes_effect_on_next_op() {
        let src = make_source();
        let deny = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) }"#).unwrap();
        src.set_acf(Some(deny)).await;

        let dummy_value = PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0));
        let ctx = make_ctx("h", "anyone", "anonymous");
        let deny_token = check(&src, "any:pv", &ctx).await;
        assert!(
            src.put_value_checked(deny_token, dummy_value.clone(), ctx.clone())
                .await
                .is_err(),
            "initial deny-WRITE policy must reject PUT"
        );

        let permissive = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) RULE(1, WRITE) }"#).unwrap();
        src.set_acf(Some(permissive)).await;

        // After swap the ACL check passes; the call now reaches the
        // upstream `cache.lookup` which fails (no upstream IOC in
        // test), so the error is a lookup/timeout — NOT the gateway
        // ACL denial. We assert the denial string is gone.
        let allow_token = check(&src, "any:pv", &ctx).await;
        let result = src.put_value_checked(allow_token, dummy_value, ctx).await;
        if let Err(msg) = result {
            assert!(
                !msg.message.contains("denied by gateway access security"),
                "post-swap PUT must NOT be ACL-denied: {msg:?}",
            );
            assert_eq!(
                msg.kind,
                OpErrorKind::Failed,
                "forwarded upstream error must classify as Failed, not a gateway denial: {msg:?}"
            );
        }
    }

    /// The gateway no longer hard-codes the ASG name to
    /// `DEFAULT`. An installed [`AsgResolver`] routes each channel
    /// through its own ASG, mirroring the per-record `ASG` field
    /// behaviour of a C IOC.
    #[tokio::test]
    async fn acl_level_uses_per_pv_asg_resolver() {
        let src = make_source();
        // Two PV namespaces — `set:*` lives under WRITE-restricted
        // OPERATOR; `dev:*` lives under READ-only LOCKED. A peer
        // without WRITE on OPERATOR may still PUT to DEFAULT.
        let cfg = parse_acf(
            r#"
UAG(admins) { admin }
ASG(DEFAULT) {
    RULE(1, READ)
    RULE(1, WRITE)
}
ASG(OPERATOR) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) }
}
ASG(LOCKED) {
    RULE(1, READ)
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        src.set_asg_resolver(Some(Arc::new(|pv: &str| {
            if pv.starts_with("set:") {
                "OPERATOR".to_string()
            } else if pv.starts_with("dev:") {
                "LOCKED".to_string()
            } else {
                "DEFAULT".to_string()
            }
        })))
        .await;

        let guest = make_ctx("anyhost", "guest", "anonymous");
        let admin = make_ctx("anyhost", "admin", "anonymous");

        // DEFAULT-routed PV: open to everyone for RW.
        assert_eq!(
            src.acl_level("other:val", &guest).await,
            AccessLevel::ReadWrite
        );

        // OPERATOR-routed PV: guest READ-only, admin RW.
        assert_eq!(
            src.acl_level("set:current", &guest).await,
            AccessLevel::Read
        );
        assert_eq!(
            src.acl_level("set:current", &admin).await,
            AccessLevel::ReadWrite
        );

        // LOCKED-routed PV: everyone READ-only (no WRITE rule).
        assert_eq!(src.acl_level("dev:hwid", &admin).await, AccessLevel::Read);
        assert_eq!(src.acl_level("dev:hwid", &guest).await, AccessLevel::Read);
    }

    /// The resolver must be hot-swappable after
    /// the gateway is wrapped behind an Arc / handed to PvaServer.
    /// Pre-fix `set_asg_resolver(&mut self)` was unreachable in the
    /// production path; this confirms the `&self` + RwLock swap
    /// works and the new policy is observed on subsequent ACL checks.
    #[tokio::test]
    async fn asg_resolver_swap_takes_effect_on_next_acl_check() {
        let src = make_source();
        let cfg = parse_acf(
            r#"
ASG(DEFAULT) {
    RULE(1, READ)
    RULE(1, WRITE)
}
ASG(LOCKED) {
    RULE(1, READ)
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        // Initial resolver: everything is DEFAULT (RW).
        let ctx = make_ctx("h", "anyone", "anonymous");
        assert_eq!(src.acl_level("X", &ctx).await, AccessLevel::ReadWrite);

        // Hot-swap: route everything to LOCKED (read-only).
        src.set_asg_resolver(Some(Arc::new(|_pv| "LOCKED".to_string())))
            .await;
        assert_eq!(src.acl_level("X", &ctx).await, AccessLevel::Read);

        // Swap back to default.
        src.set_asg_resolver(None).await;
        assert_eq!(src.acl_level("X", &ctx).await, AccessLevel::ReadWrite);
    }

    /// RPC must consult the gateway ACF too.
    /// Pre-fix every PVA RPC reached the upstream IOC carrying the
    /// gateway's identity; a `NoAccess` peer could trigger
    /// archiver-control RPCs, custom-record state changes, or any
    /// upstream action RPC.
    #[tokio::test]
    async fn rpc_ctx_denies_when_acf_no_access() {
        use epics_pva_rs::pvdata::{FieldDesc, PvField};
        let src = make_source();
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(1, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("anyhost", "intruder", "anonymous");
        let token = check(&src, "some:rpc", &ctx).await;
        let result = src
            .rpc_checked(token, FieldDesc::Variant, PvField::Null, ctx)
            .await;
        let err = result.expect_err("RPC must be denied for NoAccess peer");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "gateway ACF read-class RPC denial must classify as Denied: {err:?}"
        );
        assert!(
            err.message.contains("denied by gateway access security"),
            "denial message must name the gateway as enforcer: {err:?}",
        );
    }

    /// RPC is WRITE-class for ACF (pva2pva blocks createChannelRPC under
    /// readOnly alongside Put/Process; pvxs sharedpv has no built-in
    /// read-class RPC gate). A peer holding only READ must be denied —
    /// pre-fix the READ-class gate let a read-only peer drive
    /// state-changing RPCs on the upstream IOC.
    #[tokio::test]
    async fn rpc_denied_for_read_only_peer() {
        use epics_pva_rs::pvdata::{FieldDesc, PvField};
        let src = make_source();
        // alice has READ but no WRITE rule.
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(1, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("anyhost", "alice", "anonymous");
        let token = check(&src, "some:rpc", &ctx).await;
        let err = src
            .rpc_checked(token, FieldDesc::Variant, PvField::Null, ctx)
            .await
            .expect_err("read-only peer must be denied RPC (write-class)");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "write-class RPC denial for read-only peer must classify as Denied: {err:?}"
        );
        assert!(
            err.message.contains("denied by gateway access security"),
            "read-only RPC must be ACL-denied at the gateway: {err:?}",
        );
    }

    /// BUG 3 regression: the ctx-less `put_value` path must apply the
    /// gateway-side ACF gate, exactly like `put_value_checked`.
    /// Pre-fix `put_value` forwarded `pvput` unconditionally, so a
    /// deny-WRITE / `read_only` ACF policy was silently inert for any
    /// non-credentialed PUT.
    #[tokio::test]
    async fn bug3_put_value_denied_when_acf_no_write() {
        let src = make_source();
        // ASG with READ but no WRITE rule — every peer is denied WRITE.
        let cfg = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) }"#).unwrap();
        src.set_acf(Some(cfg)).await;

        let dummy = PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0));
        let err = src
            .put_value("any:pv", dummy)
            .await
            .expect_err("ctx-less PUT must be denied by the gateway ACF");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "ctx-less PUT gateway ACF denial must classify as Denied: {err:?}"
        );
        assert!(
            err.message.contains("denied by gateway access security"),
            "denial must name the gateway as enforcer: {err:?}",
        );
    }

    /// BUG 3: with no ACF installed (legacy pass-through), the ctx-less
    /// `put_value` is NOT ACL-denied — the gate returns ReadWrite and
    /// the call proceeds to the upstream lookup (which fails in-test
    /// with a lookup/timeout error, never the ACL denial).
    #[tokio::test]
    async fn bug3_put_value_passthrough_when_no_acf() {
        let src = make_source();
        let dummy = PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0));
        if let Err(msg) = src.put_value("any:pv", dummy).await {
            assert!(
                !msg.message.contains("denied by gateway access security"),
                "no-ACF PUT must NOT be ACL-denied: {msg:?}",
            );
            assert_eq!(
                msg.kind,
                OpErrorKind::Failed,
                "forwarded upstream error must classify as Failed, not a gateway denial: {msg:?}"
            );
        }
    }

    /// MINOR (PVA PROCESS forwarding): `process_checked` gates on the
    /// gateway ACF as a WRITE-class op. Pre-fix the gateway used the
    /// trait default `process` (`Ok(())`), silently swallowing every
    /// PROCESS and reporting false success. A non-write token must be
    /// refused with a gateway-identifying error.
    #[tokio::test]
    async fn process_checked_denied_when_acf_no_write() {
        let src = make_source();
        let cfg = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) }"#).unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("anyhost", "intruder", "anonymous");
        let token = check(&src, "some:pv", &ctx).await;
        let err = src
            .process_checked(token, ctx)
            .await
            .expect_err("PROCESS must be denied for a non-write peer");
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "PROCESS gateway ACF denial must classify as Denied: {err:?}"
        );
        assert!(
            err.message
                .contains("PROCESS denied by gateway access security"),
            "denial must name the gateway as enforcer: {err:?}",
        );
    }

    /// MINOR: with WRITE granted the `process_checked` ACL passes and
    /// the call reaches the upstream forward (which fails in-test with
    /// a lookup/timeout, never the ACL denial).
    #[tokio::test]
    async fn process_checked_passes_acl_when_write_granted() {
        let src = make_source();
        let cfg = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) RULE(1, WRITE) }"#).unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("anyhost", "anyone", "anonymous");
        let token = check(&src, "some:pv", &ctx).await;
        if let Err(msg) = src.process_checked(token, ctx).await {
            assert!(
                !msg.message.contains("denied by gateway access security"),
                "WRITE-granted PROCESS must NOT be ACL-denied: {msg:?}",
            );
            assert_eq!(
                msg.kind,
                OpErrorKind::Failed,
                "forwarded upstream error must classify as Failed, not a gateway denial: {msg:?}"
            );
        }
    }

    /// The raw-frame fast path must consult
    /// the same ACF gate as the decoded `subscribe_ctx` path. Pre-fix
    /// a NoAccess peer could mount a `subscribe_raw` subscription
    /// (since the gate covered `subscribe_ctx` only) and
    /// receive every upstream MONITOR event byte-for-byte.
    #[tokio::test]
    async fn subscribe_raw_ctx_denies_when_acf_no_access() {
        let src = make_source();
        // ASG with no fall-through rule — every UAG-less peer hits
        // NoAccess.
        let cfg = parse_acf(
            r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(1, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();
        src.set_acf(Some(cfg)).await;

        let ctx = make_ctx("anyhost", "intruder", "anonymous");
        let token = check(&src, "any:pv", &ctx).await;
        let rx = src.subscribe_raw_checked(token, ctx).await;
        assert!(
            rx.is_none(),
            "raw subscribe must be denied for a NoAccess peer"
        );
    }

    /// a downstream peer authenticating with `x509` cannot be
    /// forwarded verbatim — the pvAccess CONNECTION_VALIDATION wire
    /// has no `x509` method and the `ca` credential carries only
    /// `user`/`host` (pvxs `clientconn.cpp:217-305`). The gateway
    /// must convert the identity into a CA-style assertion *and make
    /// that conversion explicit*: the upstream `PvaClient` it builds
    /// must carry an `AssertedIdentity` recording the real downstream
    /// method + certificate authority.
    ///
    /// Pre-fix the upstream client was built with only
    /// `.user(account).host(host)` — `asserted_identity()` returned
    /// `None`, indistinguishable from a first-party `ca` login.
    #[tokio::test]
    async fn br_r8_x509_downstream_recorded_as_asserted_identity() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let src = make_source();
        let ctx = ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            account: "alice".to_string(),
            method: "x509".to_string(),
            host: "ws01.lab".to_string(),
            authority: "Lab Root CA".to_string(),
            roles: Vec::new(),
            pv_request: None,
        };
        let client = src.upstream_client_for(&ctx);
        let asserted = client
            .asserted_identity()
            .expect("x509 downstream must be recorded as a gateway-asserted identity");
        assert_eq!(
            asserted.downstream_method, "x509",
            "the real downstream auth method must be preserved for audit",
        );
        assert_eq!(
            asserted.downstream_authority, "Lab Root CA",
            "the downstream certificate authority must be preserved for audit",
        );
    }

    /// Companion: a first-party `ca` downstream is forwarded
    /// verbatim — the wire carries the same `ca` method + `user`/
    /// `host` — so the recorded `AssertedIdentity` reports `ca` with
    /// no authority. This documents that the assertion record is not
    /// a divergence for the wire-faithful case.
    #[tokio::test]
    async fn br_r8_ca_downstream_records_ca_method() {
        let src = make_source();
        let ctx = make_ctx("ws02.lab", "bob", "ca");
        let client = src.upstream_client_for(&ctx);
        let asserted = client
            .asserted_identity()
            .expect("per-credential upstream client must record the downstream identity");
        assert_eq!(asserted.downstream_method, "ca");
        assert_eq!(asserted.downstream_authority, "");
    }

    /// the gateway fans one upstream monitor (opened with the
    /// gateway's default pvRequest) out to N downstream subscribers.
    /// A downstream MONITOR pvRequest carrying a server-side `_filter`
    /// chain (pvxs `servermon.cpp:521-555`) changes *upstream event
    /// production* and cannot be honored transparently across that
    /// shared monitor, so the gateway must reject it (return `None`)
    /// rather than serve fanout events that differ from a direct
    /// upstream monitor.
    ///
    /// `pipeline` / `queueSize`, by contrast, are downstream
    /// client↔gateway flow control the gateway terminates on its own
    /// downstream connection / outbox, so they must NOT be rejected:
    /// every default-configured PVA client enables pipeline, and
    /// rejecting it would make the gateway's monitor feature unusable.
    #[tokio::test]
    async fn br_r14_server_filter_monitor_rejected_by_gateway() {
        use epics_pva_rs::server_native::MonitorOptions;

        let src = make_source();
        let ctx = make_ctx("ws03.lab", "carol", "anonymous");

        // pipeline + queueSize are downstream-local flow control —
        // transparent through the fanout gateway, NOT rejected.
        let pipeline_opts = MonitorOptions {
            pipeline: true,
            queue_size: Some(16),
            server_filter: false,
        };
        assert!(
            !pipeline_opts.affects_upstream_events(),
            "pipeline / queueSize are downstream-local and must not be \
             treated as upstream-event-affecting",
        );

        // A server-side `_filter` chain DOES change upstream event
        // production and must be rejected on both the decoded and the
        // raw fast path. The reject returns immediately — no upstream
        // lookup — so this does not block on connect_timeout.
        let filter_opts = MonitorOptions {
            pipeline: false,
            queue_size: None,
            server_filter: true,
        };
        assert!(
            filter_opts.affects_upstream_events(),
            "a server-side _filter chain must count as event-affecting",
        );
        let token = check(&src, "some:pv", &ctx).await;
        let rx = src
            .subscribe_checked_opts(token, ctx.clone(), filter_opts.clone())
            .await;
        assert!(
            rx.is_none(),
            "gateway must reject a decoded MONITOR carrying a \
             server-side _filter chain",
        );

        let raw_token = check(&src, "some:pv", &ctx).await;
        let raw_rx = src
            .subscribe_raw_checked_opts(raw_token, ctx, filter_opts)
            .await;
        assert!(
            raw_rx.is_none(),
            "gateway must reject a raw MONITOR carrying a server-side \
             _filter chain",
        );
    }

    /// Companion: an option set with no event-affecting option
    /// (`affects_upstream_events() == false`) is transparent through
    /// the gateway — the gateway does not reject it on the basis of
    /// options. (It still goes through the normal ACF + upstream
    /// lookup path.)
    #[test]
    fn br_r14_field_projection_is_not_event_affecting() {
        use epics_pva_rs::server_native::MonitorOptions;

        // The default `MonitorOptions` — what a plain `pvmonitor`
        // produces (field projection is captured as a separate
        // BitSet mask, not here) — must not be flagged as
        // event-affecting.
        let plain = MonitorOptions::default();
        assert!(
            !plain.affects_upstream_events(),
            "a plain monitor (no pipeline / queueSize / filter) must be \
             transparent through the gateway",
        );
    }

    /// GET/MONITOR must route through per-credential upstream
    /// caches so the upstream IOC's ACF sees the real downstream
    /// identity. Pre-fix the default trait impls for `get_value_checked`,
    /// `subscribe_checked`, and `subscribe_raw_checked` called the
    /// ctx-less `get_value`/`subscribe`/`subscribe_raw` which all used
    /// the single shared cache regardless of credentials.
    ///
    /// Upstream parity: pvxs p2pApp gateway source files (gw.cpp,
    /// gwserver.cpp, gwprov.cpp) are not present in this pvxs checkout
    /// (noted in doc/pvxs-functional-security-review-2026-05-18.md:27),
    /// but the wire-compatible expectation is stated in the spec:
    /// "the chosen trust boundary must be explicit" — the gateway must
    /// not silently conflate per-client upstream authorization into a
    /// single shared-client authorization.
    #[tokio::test]
    async fn br_r21_gateway_monitor_credential_scoping() {
        let src = make_source();

        let alice = make_ctx("host1", "alice", "x509");
        let bob = make_ctx("host2", "bob", "ca");

        // Two different credentials must route to SEPARATE upstream caches.
        // Each cache is backed by its own PvaClient, so the upstream IOC
        // sees the real downstream identity when applying ACF rules.
        let cache_alice = src.upstream_cache_for_test(&alice);
        let cache_bob = src.upstream_cache_for_test(&bob);
        assert!(
            !Arc::ptr_eq(&cache_alice, &cache_bob),
            "alice (x509) and bob (ca) must use distinct upstream caches \
             for per-credential upstream routing"
        );

        // Same credential must pool-share — no redundant upstream connections.
        let cache_alice2 = src.upstream_cache_for_test(&alice);
        assert!(
            Arc::ptr_eq(&cache_alice, &cache_alice2),
            "second call for same credential must reuse the existing upstream cache"
        );

        // Anonymous / empty credentials fall through to the shared gateway cache.
        let anon = make_ctx("host3", "", "anonymous");
        let cache_anon = src.upstream_cache_for_test(&anon);
        assert!(
            Arc::ptr_eq(&cache_anon, src.cache()),
            "anonymous/empty credentials must reuse the shared upstream gateway cache"
        );
    }

    /// Regression: the upstream-identity pools must be keyed
    /// by host and authority too, not only `(account, method)`.
    /// Pre-fix two downstream peers with the same account+method but
    /// different host (or x509 certificate authority) collided on the
    /// `(account, method)` key and shared the first peer's upstream
    /// PvaClient and ChannelCache — so the upstream IOC saw stale
    /// host / asserted-authority data for audit and ACF.
    #[tokio::test]
    async fn mr_r16_identity_pools_keyed_by_host_and_authority() {
        // Build a ChannelContext with an explicit authority — the
        // `make_ctx` helper always leaves authority empty.
        fn ctx_with(host: &str, account: &str, method: &str, authority: &str) -> ChannelContext {
            let mut c = make_ctx(host, account, method);
            c.authority = authority.to_string();
            c
        }

        let src = make_source();

        // Same account + method, different host.
        let alice_h1 = ctx_with("host-1", "alice", "ca", "");
        let alice_h2 = ctx_with("host-2", "alice", "ca", "");

        let client_h1 = src.upstream_client_for(&alice_h1);
        let client_h2 = src.upstream_client_for(&alice_h2);
        assert!(
            !Arc::ptr_eq(&client_h1, &client_h2),
            "same account/method from different hosts must get distinct \
             upstream clients"
        );
        let cache_h1 = src.upstream_cache_for_test(&alice_h1);
        let cache_h2 = src.upstream_cache_for_test(&alice_h2);
        assert!(
            !Arc::ptr_eq(&cache_h1, &cache_h2),
            "same account/method from different hosts must get distinct \
             upstream caches"
        );

        // Same account + method + host, different x509 authority.
        let alice_ca_a = ctx_with("host-x", "alice", "x509", "Authority-A");
        let alice_ca_b = ctx_with("host-x", "alice", "x509", "Authority-B");

        let client_a = src.upstream_client_for(&alice_ca_a);
        let client_b = src.upstream_client_for(&alice_ca_b);
        assert!(
            !Arc::ptr_eq(&client_a, &client_b),
            "same account/method/host with different certificate \
             authorities must get distinct upstream clients"
        );
        let cache_a = src.upstream_cache_for_test(&alice_ca_a);
        let cache_b = src.upstream_cache_for_test(&alice_ca_b);
        assert!(
            !Arc::ptr_eq(&cache_a, &cache_b),
            "same account/method/host with different certificate \
             authorities must get distinct upstream caches"
        );

        // Fully identical identity must still pool-share.
        let client_h1_again = src.upstream_client_for(&alice_h1);
        assert!(
            Arc::ptr_eq(&client_h1, &client_h1_again),
            "an identical identity (account, method, host, authority) \
             must reuse the pooled upstream client"
        );
        let cache_h1_again = src.upstream_cache_for_test(&alice_h1);
        assert!(
            Arc::ptr_eq(&cache_h1, &cache_h1_again),
            "an identical identity must reuse the pooled upstream cache"
        );
    }

    /// the upstream identity pools must be bounded. Pre-fix
    /// `upstream_pool` and `upstream_caches` were plain `HashMap`s;
    /// a downstream client that varied `(account, method)` on every
    /// connection could grow them without limit. Post-fix both pools
    /// cap at `max_upstream_identities` and evict the LRU entry.
    ///
    /// This test fails on main (pool.len() == 3 after 3 distinct
    /// identities with cap=2) and passes after fix (pool.len() == 2).
    #[tokio::test]
    async fn br_r7_gateway_credential_pool_bounded() {
        let src = make_source();
        // Cap both pools at 2 so the third identity triggers eviction.
        src.set_max_upstream_identities(2);

        let alice = make_ctx("h1", "alice", "ca");
        let bob = make_ctx("h2", "bob", "ca");
        let charlie = make_ctx("h3", "charlie", "ca");

        // Fill pool to cap.
        src.upstream_client_for(&alice);
        src.upstream_client_for(&bob);
        assert_eq!(
            src.upstream_pool.lock().map.len(),
            2,
            "pool must hold exactly 2 entries after 2 distinct identities"
        );

        // Access alice to make it the most-recently used.
        src.upstream_client_for(&alice);

        // Third distinct identity: must evict LRU (bob), keeping alice + charlie.
        src.upstream_client_for(&charlie);
        let pool_len = src.upstream_pool.lock().map.len();
        assert_eq!(
            pool_len, 2,
            "pool must not exceed cap=2 after a third distinct identity; got {pool_len}"
        );

        // bob (LRU) must be gone; alice and charlie must be present.
        {
            let pool = src.upstream_pool.lock();
            assert!(
                !pool.map.contains_key(&UpstreamIdentityKey::from_ctx(&bob)),
                "bob (LRU) must be evicted"
            );
            assert!(
                pool.map
                    .contains_key(&UpstreamIdentityKey::from_ctx(&alice)),
                "alice (MRU) must be retained"
            );
            assert!(
                pool.map
                    .contains_key(&UpstreamIdentityKey::from_ctx(&charlie)),
                "charlie (newest) must be retained"
            );
        }

        // Same cap applies to upstream_caches.
        src.upstream_cache_for_test(&alice);
        src.upstream_cache_for_test(&bob);
        assert_eq!(src.upstream_caches.lock().map.len(), 2);
        src.upstream_cache_for_test(&alice); // refresh alice
        src.upstream_cache_for_test(&charlie);
        let cache_len = src.upstream_caches.lock().map.len();
        assert_eq!(
            cache_len, 2,
            "upstream_caches must not exceed cap=2; got {cache_len}"
        );
    }

    /// Regression: the reported upstream-identity cap must
    /// always reflect the cap actually enforced by the pools.
    /// Pre-fix `max_upstream_identities` was a `pub` field that
    /// `set_max_upstream_identities` never updated, so the reported
    /// value desynced from the real `BoundedPool` capacity. Post-fix
    /// the accessor reads the live pool cap directly.
    #[tokio::test]
    async fn mr_r6_max_upstream_identities_tracks_setter() {
        let src = make_source();
        // Default cap.
        assert_eq!(
            src.max_upstream_identities(),
            256,
            "default cap must be 256"
        );

        // After the setter the accessor must report the new cap, not
        // the stale default.
        src.set_max_upstream_identities(2);
        assert_eq!(
            src.max_upstream_identities(),
            2,
            "accessor must reflect the cap installed by set_max_upstream_identities"
        );

        // The accessor must agree with the actual pool eviction bound.
        assert_eq!(
            src.max_upstream_identities(),
            src.upstream_pool.lock().capacity(),
            "reported cap must equal upstream_pool capacity"
        );
        assert_eq!(
            src.max_upstream_identities(),
            src.upstream_caches.lock().capacity(),
            "reported cap must equal upstream_caches capacity"
        );

        // The setter floors at 1; the accessor must reflect that too.
        src.set_max_upstream_identities(0);
        assert_eq!(
            src.max_upstream_identities(),
            1,
            "accessor must report the floored cap of 1"
        );
    }
}
