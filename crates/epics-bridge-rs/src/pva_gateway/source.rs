//! `ChannelSource` impl that bridges the gateway's [`ChannelCache`] to
//! the downstream [`epics_pva_rs::server`].
//!
//! Mirrors the role of `pva2pva GWServerChannelProvider` (server.cpp):
//! every downstream PVA op (search, get, put, monitor, get_field) is
//! resolved by looking up the PV name in the cache and forwarding to
//! the cached upstream channel. Monitor subscriptions are fanned out
//! through a per-entry tokio broadcast channel so multiple downstream
//! clients share one upstream subscription.

// RTEMS-EXEC-MODEL-ALLOW(24): checked to pass feature-ON under
// --features rtems-exec-model,pva-gateway (the gateway's spawns/timers ride the
// runtime::task seam). The default feature-ON gate omits `pva-gateway`, so re-run
// that combo when touching this module.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::OnceLock;
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
use epics_pva_rs::client::{AssertedIdentity, MarkedRead, PvaClient};
use epics_pva_rs::pvdata::{FieldDesc, PvField, RpcReply};
use epics_pva_rs::server::native_source::AcfCell;
use epics_pva_rs::server_native::source::{
    AccessChecked, ChannelContext, ChannelInvalidator, ChannelSource, MonitorStream, OpError,
    SourceRead,
};

use super::channel_cache::{
    CacheStatus, ChannelCache, DEFAULT_CLEANUP_INTERVAL, DEFAULT_MAX_ENTRIES,
};

/// An upstream read (GET or PUT_GET readback) as the downstream framing
/// contract sees it: the value plus the leaves the UPSTREAM marked.
///
/// `MarkedRead::marked` is `None` when the upstream set the root bit — it
/// framed the whole structure — which is precisely what [`SourceRead`]'s
/// `None` means downstream, so the two `None`s pass straight through. When
/// the upstream marked a subset, the gateway declares that same subset: the
/// decoder zero-filled every other leaf, and framing them as assigned would
/// ship values the upstream never sent.
fn upstream_read(read: MarkedRead) -> SourceRead {
    match read.marked {
        Some(marked) => SourceRead::marked(read.value, marked),
        None => SourceRead::from(read.value),
    }
}

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

/// The frame a subscriber gets after the gateway fanout dropped events under
/// its OWN backpressure (a tokio `broadcast::Lagged`) — a **re-frame** of the
/// channel's merged snapshot, never the next delta.
///
/// pva2pva does not forward a delta across a downstream overflow. The
/// `MonitorUser` enters overflow and folds every dropped event into one
/// coalesced element (`moncache.cpp:156-174`):
///
/// ```text
/// overrun |= lastelem->overrun            // upstream overflows
/// overrun |= changed & lastelem->changed  // downstream overflows
/// changed |= lastelem->changed            // ACCUMULATE the changed bits
/// value.copyUnchecked(lastelem->value, lastelem->changed)
/// ```
///
/// The changed BITS accumulate and the VALUES come from the merged element,
/// so a transition that happened only in a dropped event — an alarm going
/// MAJOR — still reaches the client. Forwarding the next event's delta
/// unchanged loses exactly that: the next event marks only ITS OWN leaves, and
/// on the raw path carries only its own bytes, so the alarm transition is gone
/// from the wire for good.
///
/// The broadcast ring has overwritten the dropped events, so their individual
/// changed bitsets are unrecoverable — but their EFFECT is not: the channel
/// cache's `latest` is the merge of every upstream event
/// ([`crate::pva_gateway::channel_cache::UpstreamEntry::snapshot`], the same whole-structure element a monitor seed
/// is built from). Re-framing from it delivers every accumulated value with
/// every leaf marked changed — a superset of pva2pva's accumulated bitset, and
/// the same values. No transition can be lost, and the subscriber needs no
/// "next delta is special" state to carry the loss forward.
///
/// The overrun bitset flags every leaf: after a drop, any leaf may have taken
/// an intermediate value the subscriber never saw. That is pva2pva's
/// `overrun |= changed & lastelem->changed` under a full changed set, and the
/// conservative direction — it can over-report "you may have missed a value",
/// never hide a real loss.
fn reframe_raw_body_after_lag(
    value: &PvField,
    desc: &FieldDesc,
    order: epics_pva_rs::proto::ByteOrder,
) -> Option<bytes::Bytes> {
    use epics_pva_rs::proto::BitSet;
    // pva2pva's "all changed" is the root bit (`elem->changedBitSet->set(0)`,
    // moncache.cpp:304-312). Our encoder cannot emit bit 0 (it trims to leaves,
    // as pvxs's `Value::mark` does), so the decodable equivalent is the
    // canonical FULL leaf set — the identical rule `SourceRead { marked: None }`
    // frames a seed with. Select every bit, canonicalise to the leaves.
    let mut all = BitSet::new();
    for b in 0..desc.total_bits() {
        all.set(b);
    }
    let changed = epics_pva_rs::pvdata::encode::canonical_changed_bitset(desc, &all);
    if changed.is_empty() {
        return None;
    }
    let mut body = Vec::new();
    changed.write_into(order, &mut body);
    epics_pva_rs::pvdata::encode::encode_pv_field_with_bitset(
        value, desc, &changed, 0, order, &mut body,
    );
    // overrun == changed: every leaf of the re-framed structure may have had an
    // unobserved intermediate value across the dropped range.
    changed.write_into(order, &mut body);
    Some(bytes::Bytes::from(body))
}

/// Lost-leaf paths to record in a cooked [`epics_pva_rs::server_native::source::MonitorUpdate::overrun`] after a
/// fanout broadcast lag — the port's own loss accounting, mirroring pva2pva
/// unioning the surviving element's changed leaves into the overrun bitset on
/// downstream overflow (`overrun |= changed & lastelem->changed`,
/// epics-base/modules/pva2pva/p2pApp/moncache.cpp:160-168). The cooked gateway
/// fanout re-encodes the whole value (changed = full selection mask) on every
/// event, so a dropped intermediate may have moved any leaf; the surviving
/// snapshot's changed leaves are therefore the value's own leaves, named here
/// as top-level fields. A non-structure value (no gateway PV produces one)
/// reports no overrun.
///
/// Unlike `reframe_raw_body_after_lag`, which writes into the raw body the
/// forwarder puts on the wire, this set stays SERVER-SIDE: every cooked MONITOR
/// DATA frame ends in a hard-empty overrun bitset, as pvxs's does
/// (`servermon.cpp:174-176`). See `MonitorUpdate::overrun`.
fn value_overrun_paths(value: &PvField) -> Vec<String> {
    match value {
        PvField::Structure(s) => s.fields.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    }
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
            account: ctx.creds.account.clone(),
            method: ctx.creds.method.clone(),
            host: ctx.creds.host.clone(),
            authority: ctx.creds.authority.clone(),
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
    /// Cache policy applied to every *per-credential* upstream cache
    /// built lazily in `upstream_cache_for`. The shared
    /// `cache` is constructed by the caller (`gateway.rs` /
    /// `multi_gateway.rs`) with the configured policy; these fields
    /// carry the SAME policy to credentialed caches so a site that
    /// tunes `cleanup_interval` / `max_cache_entries` for resource
    /// control gets one uniform policy across anonymous and
    /// authenticated peers. pva2pva gives
    /// each named client provider its own `ChannelCache` with the
    /// configured bound (`p2pApp/server.h:9-16`), so `max_cache_entries`
    /// is a per-cache cap here too — matching that one-cache-per-client
    /// model rather than a single global ceiling.
    pub cleanup_interval: Duration,
    /// Per-credential-cache entry ceiling — see [`Self::cleanup_interval`].
    pub per_credential_max_entries: usize,
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
    /// rules). Held in an AcfCell so policy may be hot-swapped
    /// at runtime via `set_acf`. None means pass-through (legacy
    /// behaviour) — pvxs `pva2pva` parity for sites
    /// that delegate all ACL to upstream.
    acf: AcfCell,
    /// PV→ASG resolver. Defaults to `DEFAULT` for every name; sites
    /// that want per-PV ASG granularity replace this via
    /// [`GatewayChannelSource::set_asg_resolver`].
    ///
    /// Wrapped in `RwLock` so the resolver can
    /// be hot-swapped at runtime — the gateway is typically handed
    /// off to `PvaServer` behind an `Arc` (or its trait-object
    /// equivalent), so a `&mut self` setter is unreachable after
    /// installation. Mirrors the `acf` cell's hot-swap pattern.
    asg_resolver: Arc<RwLock<AsgResolver>>,
    /// Type-state-enforced access gate. The closure
    /// captures the `asg_resolver` cell so a hot-swap of the
    /// resolver via [`GatewayChannelSource::set_asg_resolver`] is visible on the next
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
    /// Server-wide channel invalidator, wired once by
    /// the native server through [`ChannelSource::set_channel_invalidator`].
    /// Stored so it reaches not only the shared [`Self::cache`] but every
    /// per-credential cache built lazily in `upstream_cache_for` —
    /// each must publish onto the SAME server-wide invalidator so an operator
    /// `<prefix>:drop` / `:flush` disconnects downstream channels regardless
    /// of which cache held the entry. `Arc<OnceLock<…>>` so all `Clone`s of
    /// the source share one slot, consistent with the other Arc-shared
    /// fields. Unset until a server attaches (or never, for a source used
    /// outside a `PvaServer`).
    channel_invalidator: Arc<OnceLock<ChannelInvalidator>>,
}

/// The one place an upstream client-side failure becomes a downstream
/// [`OpError`] — the gateway's whole error-forwarding contract (R18-27).
///
/// An upstream op that answered with a non-success `Status` gives us that
/// `Status`; it goes downstream **as itself** — kind, message and stack —
/// because the downstream client asked the upstream question and is owed the
/// upstream answer. pva2pva gets this for free: it hands the downstream
/// requester straight to the upstream channel (`p2pApp/channel.cpp:117-127`),
/// so it has no opportunity to re-author the Status. Our two legs are separate
/// operations, so the forwarding is explicit — but it must be lossless.
/// Rendering it (`format!("{e}")`) is what put a Rust `Debug` dump on the PVA
/// wire and coerced every upstream FATAL/WARN into a downstream ERROR.
///
/// Anything else (a timeout, a dropped circuit, a decode error) is OUR
/// failure, not a forwarded one, and is authored here as a local error.
fn upstream_op_error(e: epics_pva_rs::error::PvaError) -> OpError {
    match e {
        epics_pva_rs::error::PvaError::RemoteError(status) => OpError::remote(status),
        other => OpError::failed(other.to_string()),
    }
}

impl GatewayChannelSource {
    pub fn new(cache: Arc<ChannelCache>) -> Self {
        let acf: AcfCell = epics_base_rs::server::access_security::new_acf_cell(None);
        let asg_resolver = Arc::new(RwLock::new(default_asg_resolver()));
        let gate = Self::build_gate(acf.clone(), asg_resolver.clone());
        Self {
            cache,
            connect_timeout: Duration::from_secs(5),
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
            per_credential_max_entries: DEFAULT_MAX_ENTRIES,
            subscriber_queue: 64,
            rpc_timeout: Duration::from_secs(30),
            max_subscribers: 100_000,
            subscriber_count: Arc::new(AtomicUsize::new(0)),
            upstream_pool: Arc::new(Mutex::new(BoundedPool::new(256))),
            acf,
            asg_resolver,
            gate,
            upstream_caches: Arc::new(Mutex::new(BoundedPool::new(256))),
            channel_invalidator: Arc::new(OnceLock::new()),
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
        self.acf.store(cfg.map(std::sync::Arc::new));
        // bump the gate's ACL generation — see comment on
        // `set_asg_resolver`.
        self.gate.bump_acl_version();
    }

    // the `acf_cell()` accessor was removed. It
    // returned a clone of the inner `AcfCell` so an
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
        match self.acf.load_full() {
            None => AccessLevel::ReadWrite,
            Some(cfg) => {
                // read-lock the resolver cell; the closure
                // runs while the read lock is held so the swap is
                // serialized vs. in-flight evaluations. The closure
                // itself should be O(1) — comment on `AsgResolver`
                // documents this.
                let resolver = self.asg_resolver.read().await;
                let asg = (resolver)(pv);
                cfg.check_access_method(
                    &asg,
                    &ctx.creds.host,
                    &ctx.creds.account,
                    0,
                    &ctx.creds.method,
                    "",
                )
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
        if ctx.creds.account.is_empty() || ctx.creds.method == "anonymous" {
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
        if ctx.creds.method != "ca" {
            tracing::info!(
                downstream_account = %ctx.creds.account,
                downstream_method = %ctx.creds.method,
                downstream_authority = %ctx.creds.authority,
                downstream_host = %ctx.creds.host,
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
            ctx.creds.account.clone(),
            ctx.creds.host.clone(),
            AssertedIdentity {
                downstream_method: ctx.creds.method.clone(),
                downstream_authority: ctx.creds.authority.clone(),
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
        if ctx.creds.account.is_empty() || ctx.creds.method == "anonymous" {
            return self.cache.clone();
        }
        let key = UpstreamIdentityKey::from_ctx(ctx);
        // Fast path: already in pool.
        if let Some(c) = self.upstream_caches.lock().get(&key) {
            return c.clone();
        }
        // Build outside any lock — PvaClient::builder() is pure-Rust.
        let client = self.upstream_client_for(ctx);
        // Apply the SAME configured cache policy as the shared cache —
        // not a hardcoded interval/ceiling. A site that lowers
        // `max_cache_entries` or changes `cleanup_interval` for resource
        // control now gets one uniform policy for anonymous AND
        // credentialed peers. pva2pva bounds
        // each named client's `ChannelCache` by the configured cap
        // (`p2pApp/server.h:9-16`).
        let new_cache = ChannelCache::with_max_entries(
            client,
            self.cleanup_interval,
            self.per_credential_max_entries,
        );
        // Wire the server-wide channel-invalidation sender into the
        // per-credential cache so its operator-driven `:drop`/`:flush`
        // disconnects downstream channels too, publishing onto
        // the same stream as the shared cache. Applied before publishing the
        // cache below, so it is wired no matter which racing caller wins.
        if let Some(tx) = self.channel_invalidator.get() {
            new_cache.set_invalidator(tx.clone());
        }
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

    /// Diagnostic: live subscribe-bridge tasks.
    pub fn live_subscribers(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// number of distinct upstream cache layers this source
    /// owns: the one shared (anonymous-identity) cache plus one per live
    /// credential. pva2pva reports the live client/cache count separately
    /// from the channel count (`p2pApp/server.cpp:158-175`), so the
    /// control surface can tell "many channels on one upstream" apart from
    /// "many credentialed upstreams" — the old single `cacheSize` counter
    /// conflated the two.
    pub fn upstream_cache_count(&self) -> usize {
        1 + self.upstream_caches.lock().values().len()
    }

    /// Per-cache status snapshots across the shared cache AND every
    /// per-credential upstream cache, for the control `<prefix>:report`
    /// PV. `per_cache_row_limit` caps the entry rows EACH cache
    /// contributes so a large multi-tenant gateway cannot produce an
    /// unbounded report. Mirrors pva2pva's per-client status emission
    /// (`p2pApp/server.cpp:158-230`), which walks every configured client
    /// and lists its cached channels rather than collapsing to a single
    /// global count.
    pub async fn cache_status(&self, per_cache_row_limit: usize) -> Vec<CacheStatus> {
        // Clone the per-credential caches out from under the (sync) pool
        // lock before the async `entry_status` calls, same discipline as
        // `flush_all_caches` / `total_cached_entry_count`.
        let per_cred = self.upstream_caches.lock().values();
        let mut out = Vec::with_capacity(1 + per_cred.len());
        out.push(self.cache.entry_status(per_cache_row_limit).await);
        for c in per_cred {
            out.push(c.entry_status(per_cache_row_limit).await);
        }
        out
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
        pv_request: Option<&PvField>,
    ) -> Option<(
        Option<SourceRead>,
        mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>,
    )> {
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
        let entry = match cache
            .lookup_with_request(name, pv_request, self.connect_timeout)
            .await
        {
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
        // Single-seed: the connect-time value is returned as the
        // SubscriptionSeed seed (a decoded `PvField`, since the server
        // encodes the START frame through the regular path even on the
        // raw path) and emitted by the server — NOT replayed into this
        // raw stream. pva2pva moncache.cpp:285-311 delivers `lastelem`
        // once on start(); we deliver it once via the seed. `initial_raw`
        // is still read to dedup the first broadcast event by `Bytes`
        // pointer (the snapshot/broadcast race below).
        let initial_decoded = entry.snapshot();
        let initial_raw = entry.snapshot_raw();
        // capture the Bytes pointer of the initial snapshot so the
        // broadcast loop can skip the duplicate if the same event landed in
        // both the snapshot cache and the broadcast queue (the race window
        // between subscribe_raw() and snapshot_raw() above).
        // bytes::Bytes::clone() is a refcount clone; same allocation ⟹ same
        // as_ptr(). Take the Option once so only a single event is skipped.
        let dedup_body: Option<bytes::Bytes> = initial_raw.as_ref().map(|e| e.body.clone());
        // Descriptor snapshot for bridge-local overrun marking on lag. A
        // type-change event terminates this forwarder (see the `type_changed`
        // return below), so the descriptor is stable for the forwarder's whole
        // life — capturing it once here is correct. `lookup()` waited for the
        // first upstream event, so introspection is populated; `None` only on
        // the degenerate not-yet-decoded case, where we forward unmarked.
        let desc = entry.introspection();
        let initial_order = initial_raw
            .as_ref()
            .map(|e| e.byte_order)
            .unwrap_or(epics_pva_rs::proto::ByteOrder::Little);
        let (mpsc_tx, mpsc_rx) =
            mpsc::channel::<epics_pva_rs::server_native::RawMonitorEvent>(self.subscriber_queue);
        let counter = self.subscriber_count.clone();
        epics_base_rs::runtime::task::spawn(async move {
            struct CounterGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for CounterGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let _guard = CounterGuard(counter);
            // updates-only: the cached snapshot is delivered once via the
            // SubscriptionSeed seed, not replayed here. `dedup` still
            // skips the first broadcast event that duplicates that
            // snapshot (the subscribe_raw()/snapshot_raw() race window).
            let mut dedup = dedup_body;
            // The byte order the re-frame after a lag is encoded in: the one
            // the upstream is speaking. Seeded from the cached snapshot and
            // refreshed on every event, so a synthesized frame never
            // contradicts the bodies around it.
            let mut order = initial_order;
            loop {
                match bcast.recv().await {
                    Ok(ev) => {
                        if let Some(d) = dedup.take() {
                            if ev.body.as_ptr() == d.as_ptr() {
                                continue;
                            }
                        }
                        let type_changed = ev.type_changed;
                        order = ev.byte_order;
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
                    // Events dropped under this receiver's own backpressure.
                    // Close the loss HERE, from the channel's merged snapshot —
                    // the delta that arrives next carries neither the dropped
                    // events' changed bits nor (on this raw path) their bytes,
                    // so forwarding it loses an alarm transition that happened
                    // only in a dropped event. See [`reframe_raw_body_after_lag`].
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let (Some(d), Some(snap)) = (desc.as_ref(), entry.snapshot()) else {
                            // No descriptor / no value has been decoded yet ⟹
                            // no value event has reached any subscriber, so
                            // nothing was lost to re-frame.
                            continue;
                        };
                        let Some(body) = reframe_raw_body_after_lag(&snap.value, d, order) else {
                            continue;
                        };
                        let out = epics_pva_rs::server_native::RawMonitorEvent {
                            body_bytes: body,
                            byte_order: order,
                            type_changed: false,
                        };
                        if mpsc_tx.send(out).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Some((initial_decoded, mpsc_rx))
    }

    /// Internal typed-subscribe helper. Routes `subscribe`,
    /// `subscribe_checked`, and `subscribe_seeded` through a
    /// caller-supplied cache. Returns the connect-time `snapshot` seed
    /// alongside an **updates-only** stream (the snapshot is NOT replayed
    /// into the stream); single-seed callers emit the seed themselves,
    /// legacy callers discard it.
    async fn subscribe_inner(
        &self,
        cache: Arc<ChannelCache>,
        name: &str,
        pv_request: Option<&PvField>,
    ) -> Option<(
        Option<SourceRead>,
        mpsc::Receiver<epics_pva_rs::server_native::MonitorUpdate>,
    )> {
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
        let entry = match cache
            .lookup_with_request(name, pv_request, self.connect_timeout)
            .await
        {
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
        epics_base_rs::runtime::task::spawn(async move {
            struct CounterGuard(Arc<AtomicUsize>);
            impl Drop for CounterGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Relaxed);
                }
            }
            let _guard = CounterGuard(counter);
            // updates-only: the connect-time `initial` snapshot is
            // returned as the SubscriptionSeed seed and emitted by the
            // server, NOT replayed into this stream (double-seed fix).
            //
            loop {
                match bcast_rx.recv().await {
                    Ok(update) => {
                        // A `type_changed` boundary is the last event
                        // the decoded stream carries: forward it so the server
                        // emits MONITOR FINISH, then end the forwarder —
                        // mirroring the raw forwarder's `type_changed` return.
                        let is_boundary = update.type_changed;
                        if mpsc_tx.send(update).await.is_err() {
                            return;
                        }
                        if is_boundary {
                            return;
                        }
                    }
                    // Drops under this receiver's backpressure. Close the loss
                    // HERE, by re-framing the channel's merged snapshot — the
                    // same rule as the raw path ([`reframe_raw_body_after_lag`]),
                    // and the same rule a monitor seed follows: a frame built
                    // from `latest` declares the WHOLE structure. The next
                    // event's delta marks only its own leaves, so forwarding it
                    // would drop the changed bits of everything the dropped
                    // events moved — an alarm transition among them.
                    //
                    // The lost leaves are also recorded in the cooked `overrun`
                    // set, which stays SERVER-SIDE: the cooked DATA frame ends in
                    // pvxs's hard-empty overrun bitset (`servermon.cpp:174-176`),
                    // so only a downstream on the raw path sees overrun bits.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let Some(snap) = entry.snapshot() else {
                            continue;
                        };
                        let overrun = value_overrun_paths(&snap.value);
                        let update = epics_pva_rs::server_native::MonitorUpdate {
                            value: snap.value,
                            marked: None,
                            type_changed: false,
                            overrun,
                        };
                        if mpsc_tx.send(update).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Some((initial, mpsc_rx))
    }
}

/// Adapt the internal `MonitorUpdate` fanout into a bare `PvField` stream
/// for the legacy ctx-less `subscribe` / `subscribe_checked` trait methods
/// (direct/test callers, NOT the server's wire-dispatch path). A
/// `type_changed` boundary has no representation in a bare `PvField`
/// stream, so it is dropped here — only the server's `subscribe_seeded`
/// path keeps the `MonitorUpdate` stream and turns the boundary into
/// MONITOR FINISH.
fn monitor_updates_to_values(
    mut rx: mpsc::Receiver<epics_pva_rs::server_native::MonitorUpdate>,
) -> MonitorStream<PvField> {
    let (tx, out) = mpsc::channel(64);
    epics_base_rs::runtime::task::spawn(async move {
        while let Some(update) = rx.recv().await {
            if update.type_changed {
                continue;
            }
            if tx.send(update.value).await.is_err() {
                break;
            }
        }
    });
    MonitorStream::Channel(out)
}

impl ChannelSource for GatewayChannelSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        &self.gate
    }

    fn set_channel_invalidator(&self, invalidator: ChannelInvalidator) {
        // Keep the server-wide invalidator so every per-credential cache
        // built later in `upstream_cache_for` publishes onto the same
        // invalidator, and wire it into the shared cache plus any
        // per-credential caches that already exist right now. An operator
        // `<prefix>:drop` / `:flush` then disconnects the live downstream
        // channels (server-initiated DESTROY_CHANNEL) instead of merely
        // ending their monitors — the downstream effect of pva2pva dropping a
        // `ChannelCacheEntry` (`channel->destroy()` fanout, server.cpp:133).
        if self.channel_invalidator.set(invalidator.clone()).is_err() {
            // Already wired (a second attach, e.g. the same source bound to
            // two servers). The caches hold idempotent `OnceLock`s of their
            // own; nothing to re-apply.
            return;
        }
        self.cache.set_invalidator(invalidator.clone());
        for c in self.upstream_caches.lock().values() {
            c.set_invalidator(invalidator.clone());
        }
    }

    async fn list_pvs(&self) -> Vec<String> {
        // pva2pva's `GWServerChannelProvider` overrides only `channelFind`
        // and `createChannel`, NOT `channelList` (p2pApp/server.h:9-31), so
        // the base `ChannelProvider::channelList` returns
        // `Status::error("not implemented")` with an empty name array
        // (pvAccess ChannelAccessFactory.cpp:249-258); a gateway's built-in
        // `server` `op=channels` is therefore not a PV inventory. Returning
        // the proxied cache keys here masqueraded as that inventory — a
        // partial, lazily-populated, possibly-stale snapshot of upstream PVs
        // (never-touched PVs absent, disconnected entries lingering) that
        // pva2pva never exposes. Return empty to match: the gateway proxies
        // channels, it does not own a channel list.
        //
        // The operator cache dump is still available, but through a separate
        // diagnostic surface that does not claim to be `channelList`: the
        // `<prefix>:report` control RPC (`ControlSource::build_report` →
        // `cache_status`), with per-channel connected/subscriber detail.
        Vec::new()
    }

    async fn has_pv(&self, name: &str) -> bool {
        // Admit a channel on upstream CONNECTION, not on a first MONITOR
        // event. pva2pva `p2pApp/server.cpp:36-56` answers `channelFind`
        // from `ChannelCache::lookup`, which returns an entry only once
        // `channel->isConnected()` (`chancache.cpp:166-199`) — it never
        // creates a monitor. Gating admission on the monitor cache made a
        // connectable PV that is slow to (or never does) produce a monitor
        // event look "not found", and poisoned the negative cache on a slow
        // first frame. The connect runs without a monitor (pvxs
        // `Context::connect`); the client's pool reuses the connection for
        // the GET/PUT/RPC/PROCESS that follow.
        //
        // Route the admission through the gateway-owned cache rather than
        // calling the client directly, so the connected channel is counted
        // in `cacheSize`, charged against the admission cap, and reachable
        // by `:drop`/`:flush` — pva2pva caches the channel from
        // `channelFind` (`chancache.cpp:166-199`) instead of leaving an
        // untracked connection.
        self.cache
            .admit_connection(name, self.connect_timeout)
            .await
            .is_ok()
    }

    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        // Forward a real upstream GET_FIELD rather than reading the monitor
        // cache descriptor. pva2pva forwards introspection through the
        // connected channel independently of monitor setup
        // (`p2pApp/channel.cpp:99-148`); `pvinfo` issues a one-shot
        // GET_FIELD that reuses the client's connection pool and no longer
        // waits on a first monitor event (see `has_pv`).
        epics_base_rs::runtime::task::timeout(
            self.connect_timeout,
            self.cache.client().pvinfo(name),
        )
        .await
        .ok()?
        .ok()
    }

    /// a credentialed downstream CREATE_CHANNEL resolves existence by
    /// CONNECTING through THAT peer's per-credential upstream client, not
    /// by waiting on a monitor event, so channel setup never opens upstream
    /// monitor state under the shared identity and a connectable-but-not-
    /// monitorable PV still resolves (see `has_pv` for the pva2pva
    /// `server.cpp:36-56` / `chancache.cpp:166-199` rationale). Anonymous
    /// peers fall back to the shared client (`upstream_client_for` returns
    /// `self.cache.client()`). The credential-free `has_pv` is kept for the
    /// connectionless UDP SEARCH path, which carries no identity.
    ///
    /// Admission routes through THAT peer's per-credential cache
    /// (`upstream_cache_for`, backed by `upstream_client_for`), so the
    /// connection-only channel counts in that cache's `cacheSize` and is
    /// reachable by the all-layers `:drop`/`:flush` — the same accounting
    /// `has_pv` gets on the shared cache.
    async fn has_pv_checked(&self, name: &str, ctx: ChannelContext) -> bool {
        self.upstream_cache_for(&ctx)
            .admit_connection(name, self.connect_timeout)
            .await
            .is_ok()
    }

    /// descriptor discovery for a credentialed downstream peer
    /// (CREATE_CHANNEL GET-INIT / GET_FIELD) forwards a one-shot upstream
    /// GET_FIELD through that peer's per-credential client — the same
    /// identity routing `get_value_checked` uses — so the upstream audit
    /// identity matches the operation that follows, and discovery no longer
    /// waits on a monitor event (see `get_introspection`).
    async fn get_introspection_checked(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Option<FieldDesc> {
        epics_base_rs::runtime::task::timeout(
            self.connect_timeout,
            self.upstream_client_for(&ctx).pvinfo(name),
        )
        .await
        .ok()?
        .ok()
    }

    async fn get_value(&self, name: &str) -> Option<PvField> {
        // Forward a real upstream ChannelGet rather than serving the cached
        // monitor snapshot. pva2pva `p2pApp/channel.cpp:109-115` implements
        // GET as `entry->channel->createChannelGet(channelGetRequester,
        // pvRequest)`; the monitor cache (`moncache.cpp`) feeds downstream
        // MONITOR fanout only and never answers a ChannelGet. Replaying the
        // snapshot returned a value that could be stale at GET time, skipped
        // any upstream GET-side processing, and forced non-monitorable PVs
        // through the monitor path. `pvget` issues a one-shot GET that reuses
        // the client's connection pool, so no extra upstream search is paid.
        self.cache.client().pvget(name).await.ok()
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
        // No monitor-gated preflight. The PUT forwards through the shared
        // client, which connects on demand and surfaces the upstream error
        // itself — exactly as `put_value_checked` does. pva2pva forwards
        // PUT through the connected upstream channel independently of
        // monitor setup (`p2pApp/channel.cpp:99-148`); a monitor-cache
        // lookup here made a write wait on a first monitor event (see
        // `has_pv`).
        // typed pass-through — forward the PvField as-is without
        // re-encoding through string form. pvxs serialises the PUT value
        // with to_wire_valid(R, temp) (pvxs/src/clientget.cpp:305) — no
        // string round-trip in the reference implementation.
        self.cache
            .client()
            .pvput_pv_field(name, &value)
            .await
            .map_err(upstream_op_error)
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
                account = %ctx.creds.account,
                method = %ctx.creds.method,
                "pva-gateway: PUT denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PUT denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
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
            account = %ctx.creds.account,
            method = %ctx.creds.method,
            "pva-gateway: forwarding PUT with downstream credentials"
        );
        // Forward the downstream PUT INIT pvRequest — preserved into
        // `ctx.pv_request` (PUT INIT stash) — so upstream
        // `record._options.process`/`block` are honored. pva2pva forwards
        // `createChannelPut(..., pvRequest)` verbatim (channel.cpp:117-127);
        // the prior `pvput_pv_field` reissued the gateway default request,
        // dropping process/block. The data leg still targets the `value`
        // bit (typed pass-through, see put_value). Atomic single-upstream
        // PUT_GET / delta forwarding (one upstream op carrying the
        // readback) is a separate, larger change. Falls back to the
        // default request when the downstream sent none.
        let result = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvput_pv_field_with_request_value(name, req, &value)
                    .await
            }
            None => client.pvput_pv_field(name, &value).await,
        };
        result.map_err(upstream_op_error)
    }

    /// Credential-aware atomic **PUT_GET** — forward the whole operation
    /// as ONE upstream PVA `ChannelPutGet` and return its upstream
    /// readback, instead of the default local put-then-(cached)-get.
    ///
    /// pva2pva `p2pApp/channel.cpp:129-137` implements
    /// `GWChannel::createChannelPutGet` by forwarding
    /// `createChannelPutGet(..., pvRequest)` verbatim to the upstream
    /// channel: the upstream IOC applies the put and produces the readback
    /// atomically under the downstream's own pvRequest
    /// (`record._options.process`/`block`). The default
    /// `put_delta_checked` + `get_value_checked` composition instead merged
    /// the delta against this gateway's cached monitor snapshot and read the
    /// post-put value back from that same cache, losing upstream atomicity
    /// and the original request. Routing through the per-(account, method)
    /// `upstream_client_for` keeps the downstream identity, exactly as
    /// `put_value_checked`/`get_value_checked`/`rpc_checked` do. Plain GET
    /// still returns the cached snapshot — this override is the only place
    /// the PUT_GET readback diverges from that cache.
    async fn put_get_checked(
        &self,
        checked: AccessChecked,
        _desc: std::sync::Arc<FieldDesc>,
        _changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> Result<Option<SourceRead>, OpError> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.creds.account,
                method = %ctx.creds.method,
                "pva-gateway: PUT_GET denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PUT_GET denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }

        let name = checked.pv_name();
        let client = self.upstream_client_for(&ctx);
        tracing::debug!(
            pv = %name,
            account = %ctx.creds.account,
            method = %ctx.creds.method,
            "pva-gateway: forwarding PUT_GET with downstream credentials"
        );
        // One upstream PUT_GET carrying the downstream's preserved INIT
        // pvRequest (process/block) and the put value; the value targets the
        // `value` bit (typed pass-through, matching the plain-PUT path) and
        // the upstream post-put readback is returned verbatim — INCLUDING the
        // leaves the upstream marked in it (`MarkedRead::marked`), so the
        // downstream reply frames the upstream's changed-bitset rather than a
        // full mask over decoder-zero-filled leaves (pva2pva forwards the
        // upstream readback's bitset, `p2pApp/channel.cpp:129-137`). Falls
        // back to the default request when the downstream sent none.
        let read = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvput_get_pv_field_with_request_value_marked(name, req, delta)
                    .await
            }
            None => client.pvput_get_pv_field_marked(name, delta).await,
        };
        read.map(|r| Some(upstream_read(r)))
            .map_err(upstream_op_error)
    }

    async fn is_writable(&self, name: &str) -> bool {
        // Proxy the UPSTREAM channel's connection state, not the local cache.
        //
        // pva2pva `GWChannel::getAccessRights` delegates to the connected
        // upstream channel — `return entry->channel->getAccessRights(pvField)`
        // (modules/pva2pva/p2pApp/channel.cpp:92-96) — so a gateway reports
        // the proxied upstream's rights, never a gateway-local flag.
        // Reporting mere monitor-cache presence advertised "writable"
        // whenever any prior op had populated the cache (and "not writable"
        // for a connectable PV not yet cached), independent of the upstream's
        // actual access — wrong on both edges.
        //
        // PVA transmits no per-field access-rights message and this client
        // exposes no per-field writability, so the faithful, physically-
        // available proxy is the upstream channel's CONNECTION: a channel the
        // gateway can connect to is the one whose rights pva2pva would
        // forward. Route the probe through the gateway-owned cache (the same
        // accounting as `has_pv`): a connectable PV counts in `cacheSize`, is
        // charged against the admission cap, and is reachable by
        // `:drop`/`:flush`, so repeated probes reuse the one cached channel
        // rather than opening an untracked connection per call.
        self.cache
            .admit_connection(name, self.connect_timeout)
            .await
            .is_ok()
    }

    /// Forward an RPC request through the upstream client. The default
    /// trait impl returns "RPC Not Implemented", which is a major p2pApp
    /// parity gap (review §1). With this override, RPC requests pass
    /// through transparently — `pvrpc` reuses the cached channel
    /// connection-pool entry so we don't pay a fresh search per call.
    async fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<RpcReply, OpError> {
        // No monitor-gated preflight (see `put_value`): the RPC forwards
        // through the shared client, which connects on demand and surfaces
        // the upstream error itself.
        let result = epics_base_rs::runtime::task::timeout(
            self.rpc_timeout,
            self.cache
                .client()
                .pvrpc(name, &request_desc, &request_value),
        )
        .await;
        match result {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(upstream_op_error(e)),
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
    ) -> Result<RpcReply, OpError> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.creds.account,
                method = %ctx.creds.method,
                "pva-gateway: RPC denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "RPC denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        // no shared-cache preflight (see put_value_checked).
        // The RPC runs through the per-credential client, which resolves
        // existence and returns the upstream error itself.
        let client = self.upstream_client_for(&ctx);
        // Forward the downstream RPC INIT pvRequest — captured into
        // `ctx.pv_request` at RPC INIT — as the upstream
        // create-time pvRequest, distinct from the EXEC argument
        // (`request_desc`/`request_value`). pva2pva forwards the original
        // create-time pvRequest into `createChannelRPC(..., pvRequest)`
        // (channel.cpp:140-148); the prior code reissued the argument as
        // the upstream INIT pvRequest, so a provider that interprets
        // create-time fields saw the wrong request shape. Fall back to the
        // default empty pvRequest when the downstream sent none.
        let call = async {
            match ctx.pv_request.as_ref() {
                Some(req) => {
                    let req_desc = req.descriptor();
                    client
                        .pvrpc_with_request(name, &req_desc, req, &request_desc, &request_value)
                        .await
                }
                None => client.pvrpc(name, &request_desc, &request_value).await,
            }
        };
        let result = epics_base_rs::runtime::task::timeout(self.rpc_timeout, call).await;
        match result {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(upstream_op_error(e)),
            Err(_) => Err(OpError::failed(format!("upstream rpc timeout for {name}"))),
        }
    }

    /// Forward a PVA PROCESS (wire cmd 16) to the upstream PV. The
    /// `ChannelSource` default returns `Ok(())` — for a proxying
    /// gateway that silently swallows the PROCESS and falsely reports
    /// success, so a downstream `caput -c` / `pvcall .PROC` never
    /// actually triggers the upstream record. This override forwards
    /// `pvprocess` through the shared client, which connects on demand.
    async fn process(&self, name: &str) -> Result<(), OpError> {
        // No monitor-gated preflight (see `put_value`): PROCESS forwards
        // through the shared client, which connects on demand and surfaces
        // the upstream error itself.
        self.cache
            .client()
            .pvprocess(name)
            .await
            .map_err(upstream_op_error)
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
                account = %ctx.creds.account,
                method = %ctx.creds.method,
                "pva-gateway: PROCESS denied by gateway ACF"
            );
            return Err(OpError::denied(format!(
                "PROCESS denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        // no shared-cache preflight (see put_value_checked).
        // PROCESS runs through the per-credential client, which resolves
        // existence and returns the upstream error itself.
        let client = self.upstream_client_for(&ctx);
        // Forward the downstream PROCESS INIT pvRequest — captured into
        // `ctx.pv_request` at PROCESS INIT — upstream, matching
        // pva2pva createChannelProcess(..., pvRequest) (channel.cpp:98-106).
        // The prior code validated but discarded it, so an upstream
        // provider that interprets PROCESS `record._options` saw the
        // gateway's default request. Fall back to the default empty
        // pvRequest when the downstream sent none.
        let result = match ctx.pv_request.as_ref() {
            Some(req) => client.pvprocess_with_request_value(name, req).await,
            None => client.pvprocess(name).await,
        };
        result.map_err(upstream_op_error)
    }

    // ── ChannelArray (cmd 14) — forward to the per-credential upstream ──
    //
    // pva2pva `GWChannel::createChannelArray` (`channel.cpp:227-232`)
    // forwards the array op to the upstream channel with the downstream's
    // pvRequest unchanged. We mirror that: every sub-op runs through the
    // per-credential client (so the upstream IOC sees the real downstream
    // identity, like `put_value_checked` / `rpc_checked` / `process_checked`)
    // and carries the downstream INIT pvRequest — captured in
    // `ctx.pv_request` — so the upstream resolves the same bound array field
    // the downstream selected. Each forward is one upstream
    // INIT+sub-op+DESTROY (the gateway holds no upstream array op state),
    // matching the stateless one-shot forward the other write/process paths
    // use. ACF is applied per sub-op by the read/write gate below, mirroring
    // pvAccessCPP's per-sub-op gating (`responseHandlers.cpp` gates each
    // ChannelArray request, not the create).

    async fn channel_array_init(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> Result<FieldDesc, OpError> {
        // INIT only resolves the bound array field's introspection — no
        // access check here, exactly as the native server gates per sub-op
        // rather than at create. Forward an upstream describe carrying the
        // downstream pvRequest so the reported descriptor is the upstream's.
        let client = self.upstream_client_for(&ctx);
        let result = match ctx.pv_request.as_ref() {
            Some(req) => client.pvarray_describe_with_request_value(name, req).await,
            None => client.pvarray_describe(name).await,
        };
        result.map_err(upstream_op_error)
    }

    async fn channel_array_get(
        &self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> Result<PvField, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied(format!(
                "ARRAY getArray denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        let client = self.upstream_client_for(&ctx);
        let result = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvarray_get_with_request_value(name, req, offset, count, stride)
                    .await
            }
            None => client.pvarray_get(name, offset, count, stride).await,
        };
        result
            .map(|(_desc, value)| value)
            .map_err(upstream_op_error)
    }

    async fn channel_array_put(
        &self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied(format!(
                "ARRAY putArray denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        let client = self.upstream_client_for(&ctx);
        let result = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvarray_put_with_request_value(name, req, &value, offset, stride)
                    .await
            }
            None => client.pvarray_put(name, &value, offset, stride).await,
        };
        result.map_err(upstream_op_error)
    }

    async fn channel_array_set_length(
        &self,
        checked: AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied(format!(
                "ARRAY setLength denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        let client = self.upstream_client_for(&ctx);
        let result = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvarray_set_length_with_request_value(name, req, length)
                    .await
            }
            None => client.pvarray_set_length(name, length).await,
        };
        result.map_err(upstream_op_error)
    }

    async fn channel_array_get_length(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Result<u32, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied(format!(
                "ARRAY getLength denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.creds.host,
                account = ctx.creds.account,
                method = ctx.creds.method,
            )));
        }
        let name = checked.pv_name();
        let client = self.upstream_client_for(&ctx);
        let result = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvarray_get_length_with_request_value(name, req)
                    .await
            }
            None => client.pvarray_get_length(name).await,
        };
        result.map_err(upstream_op_error)
    }

    async fn subscribe_raw(
        &self,
        name: &str,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::RawMonitorEvent>> {
        // legacy ctx-less path: updates-only, seed discarded (the
        // single-seed server path uses `subscribe_raw_seeded`).
        self.subscribe_raw_inner(self.cache.clone(), name, None)
            .await
            .map(|(_initial, updates)| MonitorStream::Channel(updates))
    }

    async fn subscribe(&self, name: &str) -> Option<MonitorStream<PvField>> {
        // legacy ctx-less path: updates-only, seed discarded (the
        // single-seed server path uses `subscribe_seeded`). The internal
        // stream is `MonitorUpdate`; adapt to bare `PvField` for this
        // legacy signature (boundary dropped — not a wire-dispatch path).
        self.subscribe_inner(self.cache.clone(), name, None)
            .await
            .map(|(_initial, updates)| monitor_updates_to_values(updates))
    }

    /// route GET through the per-credential upstream client so the
    /// upstream IOC sees the real downstream identity. Pre-fix this
    /// served `entry.snapshot()` from the monitor cache, which returned
    /// stale cached state instead of forwarding a real upstream ChannelGet
    /// (see `get_value` for the pva2pva `channel.cpp:109-115` rationale).
    /// The GET / PUT_GET-readback framing path: forward the upstream read
    /// AND the leaves the upstream marked in it.
    ///
    /// The trait default wraps [`Self::get_value_checked`] as
    /// `marked: None` — "the source assigned everything" — which for a
    /// gateway is false: the upstream frames only the leaves ITS source
    /// assigned (`to_wire_valid`), and the client decoder zero-fills the
    /// rest. Re-framing a full mask downstream would put those fabricated
    /// zeros on the wire (e.g. the seven `getProperties` never assigns).
    /// pva2pva forwards the upstream reply's own bitset
    /// (`p2pApp/channel.cpp:109-114`), which is what the upstream marks
    /// carried here reproduce. A root-bit reply (upstream said "whole
    /// structure") arrives as `None` and stays `None`.
    async fn read_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<SourceRead> {
        if !checked.allows_read() {
            return None;
        }
        let client = self.upstream_client_for(&ctx);
        let read = match ctx.pv_request.as_ref() {
            Some(req) => {
                client
                    .pvget_pv_field_with_request_value_marked(checked.pv_name(), req)
                    .await
            }
            None => client.pvget_marked(checked.pv_name()).await,
        };
        read.ok().map(upstream_read)
    }

    async fn get_value_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<PvField> {
        if !checked.allows_read() {
            return None;
        }
        // Forward a real upstream ChannelGet through the per-credential
        // client so the upstream IOC sees the real downstream identity and
        // owns GET freshness — the same identity-preserving routing
        // `put_value_checked` / `rpc_checked` / `process_checked` already use.
        // pva2pva `channel.cpp:109-115` forwards createChannelGet rather than
        // replaying the monitor cache; the snapshot path returned stale cache
        // state and never ran the upstream GET-side handler.
        //
        // Carry the downstream's preserved pvRequest upstream when one was
        // sent, so a `field(...)` projection or `record._options` set on the
        // downstream GET reaches the upstream IOC verbatim — exactly what
        // `GWChannel::createChannelGet` does, forwarding the requester's
        // pvRequest to the upstream `createChannelGet`
        // (epics-base/modules/pva2pva/p2pApp/channel.cpp:110-114). Falls back
        // to the default request when the downstream sent none, matching the
        // PUT_GET readback path above.
        let client = self.upstream_client_for(&ctx);
        match ctx.pv_request.as_ref() {
            Some(req) => client
                .pvget_pv_field_with_request_value(checked.pv_name(), req)
                .await
                .map(|(_desc, value)| value)
                .ok(),
            None => client.pvget(checked.pv_name()).await.ok(),
        }
    }

    /// route MONITOR through per-credential upstream cache.
    async fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<PvField>> {
        if !checked.allows_read() {
            return None;
        }
        self.subscribe_inner(
            self.upstream_cache_for(&ctx),
            checked.pv_name(),
            ctx.pv_request.as_ref(),
        )
        .await
        .map(|(_initial, updates)| monitor_updates_to_values(updates))
    }

    /// route raw MONITOR through per-credential upstream cache.
    async fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::RawMonitorEvent>> {
        if !checked.allows_read() {
            return None;
        }
        self.subscribe_raw_inner(
            self.upstream_cache_for(&ctx),
            checked.pv_name(),
            ctx.pv_request.as_ref(),
        )
        .await
        .map(|(_initial, updates)| MonitorStream::Channel(updates))
    }

    /// decoded MONITOR with the downstream's event-affecting
    /// pvRequest options.
    ///
    /// The gateway no longer fans a single default-pvRequest upstream
    /// monitor out to every subscriber for a PV name. Following pva2pva
    /// (`p2pApp/channel.cpp:157-193`, `p2pApp/moncache.cpp:34-37,56-81`),
    /// the upstream monitor cache is keyed by `(pv_name, serialized
    /// pvRequest)`: each distinct downstream pvRequest opens — and shares
    /// — its own upstream monitor, opened with that exact pvRequest. A
    /// server-side `_filter` chain, `field(...)` projection, and the
    /// `record._options` (`queueSize`, `pipeline`) all travel verbatim to
    /// the upstream IOC, which produces the event stream the option set
    /// asks for; subscribers with an identical pvRequest still coalesce
    /// onto one upstream monitor.
    ///
    /// Carrying the request upstream removes the dual meaning that forced
    /// the old reject-gate: there is no longer a "default fanout" stream
    /// whose events diverge from what an event-affecting option demands,
    /// so nothing has to be rejected. `opts` is therefore redundant here —
    /// the full request is forwarded structurally via `ctx.pv_request`
    /// (threaded through `subscribe_checked` → `subscribe_inner` →
    /// `ChannelCache::lookup_with_request`).
    async fn subscribe_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<PvField>> {
        // Delegate to the ACF-gated `subscribe_checked` path, which
        // forwards `ctx.pv_request` to a per-pvRequest upstream monitor.
        // The cooked `subscribe_checked_opts_marked` variant the server
        // dispatches on routes through this method via the trait default
        // and wraps each value with `marked: None`, like every non-group
        // source.
        self.subscribe_checked(checked, ctx).await
    }

    /// Raw-path counterpart of [`Self::subscribe_checked_opts`]. Same
    /// per-pvRequest upstream-monitor forwarding via `ctx.pv_request`.
    async fn subscribe_raw_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<MonitorStream<epics_pva_rs::server_native::RawMonitorEvent>> {
        self.subscribe_raw_checked(checked, ctx).await
    }

    /// Single-seed MONITOR (the server's monitor START dispatch path).
    /// The gateway seeds the monitor from its shared upstream-monitor
    /// CACHED snapshot, captured atomically with subscriber registration
    /// inside `subscribe_inner` — matching pvxs/pva2pva, which copy one
    /// cached `lastelem` per `start()` (`moncache.cpp:270-320`). The
    /// trait default would instead seed via `get_value_checked`, which
    /// for the gateway forwards a FRESH upstream ChannelGet (see
    /// `get_value_checked` — correct for a standalone GET, wrong for a
    /// monitor seed) AND would double-seed against this stream. `_opts`
    /// is redundant: the full request travels upstream via
    /// `ctx.pv_request`.
    async fn subscribe_seeded(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::source::SubscriptionSeed<
            epics_pva_rs::server_native::source::MonitorUpdate,
        >,
    > {
        if !checked.allows_read() {
            return None;
        }
        let (initial, updates) = self
            .subscribe_inner(
                self.upstream_cache_for(&ctx),
                checked.pv_name(),
                ctx.pv_request.as_ref(),
            )
            .await?;
        // `subscribe_inner` already yields a `MonitorUpdate` stream that
        // carries the `type_changed` boundary; forward it as the
        // seed's update stream so the server's decoded loop emits MONITOR
        // FINISH on an upstream descriptor change.
        // The gateway suspends its single upstream subscription per
        // `(name, ctx)` cache layer via `notify_monitor_start`, not per
        // downstream op, so no per-op `MonitorGate` here.
        Some(epics_pva_rs::server_native::source::SubscriptionSeed {
            // The seed is the cache's merged upstream snapshot, declared
            // whole-structure — pva2pva's `changedBitSet->set(0)` on the
            // element it hands a starting MonitorUser (`moncache.cpp:304-312`),
            // rendered as the canonical full leaf bitset because our encoder
            // cannot emit the root bit. `channel_cache::monitor_seed` owns that
            // rule; nothing here may re-declare it. Post-seed updates are the
            // other rule — they carry the upstream event's own changed bitset
            // through `MonitorUpdate::marked` (`moncache.cpp:142`).
            initial,
            updates: MonitorStream::Channel(updates),
            on_start: None,
        })
    }

    /// Raw-path counterpart of [`Self::subscribe_seeded`]. Same cached
    /// snapshot seed (decoded — the server encodes the START frame
    /// through the regular path even on the raw path) captured
    /// atomically with raw subscriber registration.
    async fn subscribe_raw_seeded(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::source::SubscriptionSeed<
            epics_pva_rs::server_native::RawMonitorEvent,
        >,
    > {
        if !checked.allows_read() {
            return None;
        }
        let (initial, updates) = self
            .subscribe_raw_inner(
                self.upstream_cache_for(&ctx),
                checked.pv_name(),
                ctx.pv_request.as_ref(),
            )
            .await?;
        Some(epics_pva_rs::server_native::source::SubscriptionSeed {
            // Same contract as `subscribe_seeded`: the seed is whole-structure
            // (`channel_cache::monitor_seed`). The raw path's seed is encoded
            // through the decoded path, so it goes on the wire with exactly
            // that bitset.
            initial,
            updates: MonitorStream::Channel(updates),
            on_start: None,
        })
    }

    /// The gateway declares NO pipeline-window watermarks, and therefore
    /// never asks the server for pause/resume callbacks (the trait default
    /// `notify_watermark` is a no-op).
    ///
    /// A downstream monitor must not throttle the upstream: that upstream is
    /// shared with every co-subscriber of the same (PV, credential), and a
    /// pause is not divisible. pva2pva does not throttle either — its
    /// `MonitorCacheEntry::notify` polls the upstream dry with a bare
    /// `//TODO: flow control` where an upstream throttle would go
    /// (`moncache.cpp:133-137`) and absorbs a slow downstream in that
    /// downstream's OWN `overflowElement`, counting the coalesced updates in
    /// `ndropped` (`:151-174`).
    ///
    /// Declaring `Some((0, 0))` here is what made the R18-25 starvation
    /// reachable: it turned every downstream DATA emission into a pause vote
    /// on the shared upstream, and an op became a *voter* only by crossing LOW
    /// itself — so a non-pipelined co-subscriber could neither vote nor undo
    /// the pause, and starved. The overflow absorption the gateway already has
    /// (broadcast lag → `pending_overrun` → the overrun mark on the next body,
    /// see `subscribe_raw_inner` / `subscribe_inner`) IS pva2pva's mechanism,
    /// and it needs no upstream throttle.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        let _ = name;
        None
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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: account.to_string(),
                method: method.to_string(),
                host: host.to_string(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
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
            .check(
                pv,
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
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

    /// `set_channel_invalidator` wires the server-wide invalidator
    /// into the shared cache AND every per-credential cache built afterward,
    /// so an operator `<prefix>:drop` routed through the gateway publishes
    /// the dropped name — the downstream effect being a server-initiated
    /// DESTROY_CHANNEL of the live channel, not just a silently-ended
    /// monitor. Asserts both cache layers publish onto the one invalidator.
    #[tokio::test]
    async fn set_channel_invalidator_wires_shared_and_per_credential_caches() {
        let src = make_source();
        let inv = ChannelInvalidator::new();
        let mut rx = inv.subscribe();
        src.set_channel_invalidator(inv.clone());

        // Shared (anonymous) cache publishes on a drop routed through the
        // gateway-level all-caches API the control RPC uses. Each drop
        // publishes one batch carrying the removed name(s).
        src.cache.insert_test_entry("SHARED:PV").await;
        assert!(src.drop_entry_all_caches("SHARED:PV").await);
        assert_eq!(
            rx.try_recv().expect("shared-cache drop published").to_vec(),
            vec!["SHARED:PV".to_string()]
        );

        // A per-credential cache built AFTER wiring inherits the SAME
        // invalidator (upstream_cache_for applies the stored handle to fresh
        // caches), so a credentialed drop publishes onto the same invalidator.
        let ctx = make_ctx("h", "alice", "ca");
        let cred_cache = src.upstream_cache_for_test(&ctx);
        cred_cache.insert_test_entry("CRED:PV").await;
        assert!(src.drop_entry_all_caches("CRED:PV").await);
        assert_eq!(
            rx.try_recv()
                .expect("per-credential-cache drop published")
                .to_vec(),
            vec!["CRED:PV".to_string()]
        );
    }

    /// pva2pva exposes no `channelList`, so a gateway's `server`
    /// `op=channels` must not be a PV inventory. `list_pvs()` must return
    /// empty even when the cache holds proxied upstream PVs — those names
    /// are visible only through the `<prefix>:report` diagnostic, not as
    /// channel inventory.
    #[tokio::test]
    async fn list_pvs_does_not_expose_cached_upstream_pvs() {
        let src = make_source();
        src.cache.insert_test_entry("UPSTREAM:PV").await;
        // The entry really is cached (so the empty list_pvs is a
        // deliberate hide, not an empty cache).
        assert!(
            src.cache.names().await.contains(&"UPSTREAM:PV".to_string()),
            "test entry must be present in the cache"
        );
        assert!(
            src.list_pvs().await.is_empty(),
            "gateway list_pvs must not masquerade the cache as channelList (pva2pva parity)"
        );
    }
    /// R18-25: the gateway declares NO pipeline watermarks, so the server
    /// never asks it to pause a shared upstream. pva2pva does not throttle
    /// upstream either (`moncache.cpp:133-137`, the `//TODO: flow control`);
    /// a slow downstream is absorbed in its own overflow element.
    ///
    /// Pre-fix this returned `Some((0, 0))`, which turned every downstream
    /// DATA emission into a pause vote on the shared upstream and let one slow
    /// pipelined client starve every co-subscriber.
    #[tokio::test]
    async fn r18_25_gateway_declares_no_pipeline_watermarks() {
        let src = make_source();
        assert_eq!(
            <GatewayChannelSource as ChannelSource>::monitor_watermarks(&src, "ANY:PV").await,
            None,
            "a downstream monitor must never throttle the shared upstream"
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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "alice".to_string(),
                method: "x509".to_string(),
                host: "ws01.lab".to_string(),
                authority: "Lab Root CA".to_string(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
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

    /// An option set with no event-affecting option
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
            std::sync::Arc::make_mut(&mut c.creds).authority = authority.to_string();
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

    // ---- re-framing a lagged subscriber from the merged snapshot -------

    use epics_pva_rs::proto::{BitSet, ByteOrder};
    use epics_pva_rs::pvdata::encode::decode_pv_field_with_bitset;
    use epics_pva_rs::pvdata::{ScalarType, ScalarValue};

    /// R19-44: the frame a lagged subscriber gets is a RE-FRAME of the merged
    /// snapshot — every leaf present, every leaf marked changed — not the next
    /// delta. This is the boundary the defect lived on: a leaf that moved ONLY
    /// in a dropped event (an alarm going MAJOR) must still reach the client,
    /// and a delta cannot carry it (pva2pva accumulates the changed bits and
    /// copies the merged values instead, `moncache.cpp:156-174`).
    #[test]
    fn lag_reframes_every_leaf_of_the_merged_snapshot() {
        // Structure { a: Int, b: Int }: bit 0 = struct, bit 1 = a, bit 2 = b.
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        // The merged snapshot: `b` moved in an event this subscriber never saw.
        let merged = {
            let mut s = epics_pva_rs::pvdata::PvStructure::new("");
            s.fields
                .push(("a".to_string(), PvField::Scalar(ScalarValue::Int(7))));
            s.fields
                .push(("b".to_string(), PvField::Scalar(ScalarValue::Int(9))));
            PvField::Structure(s)
        };

        let body =
            reframe_raw_body_after_lag(&merged, &desc, ByteOrder::Little).expect("re-framed body");

        let mut cur = std::io::Cursor::new(body.as_ref());
        let changed = BitSet::decode(&mut cur, ByteOrder::Little).expect("changed");
        assert!(
            changed.get(1) && changed.get(2),
            "every leaf is marked changed"
        );
        let decoded =
            decode_pv_field_with_bitset(&desc, &changed, 0, &mut cur, ByteOrder::Little).unwrap();
        assert_eq!(
            decoded, merged,
            "the dropped events' values ride the re-frame; a delta would carry only its own leaf"
        );

        // Loss is still signalled: after a drop any leaf may have taken an
        // intermediate value the subscriber never observed.
        let overrun = BitSet::decode(&mut cur, ByteOrder::Little).expect("overrun");
        assert!(
            overrun.get(1) && overrun.get(2),
            "every re-framed leaf is flagged overrun: {overrun:?}"
        );
    }

    /// A scalar channel has one leaf and no struct bit — the same rule, on the
    /// degenerate shape.
    #[test]
    fn lag_reframes_a_scalar_channel() {
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let value = PvField::Scalar(ScalarValue::Double(42.0));

        let body =
            reframe_raw_body_after_lag(&value, &desc, ByteOrder::Little).expect("re-framed body");

        let mut cur = std::io::Cursor::new(body.as_ref());
        let changed = BitSet::decode(&mut cur, ByteOrder::Little).expect("changed");
        assert!(changed.get(0), "the scalar leaf is marked changed");
        let decoded =
            decode_pv_field_with_bitset(&desc, &changed, 0, &mut cur, ByteOrder::Little).unwrap();
        assert_eq!(decoded, value);
        let overrun = BitSet::decode(&mut cur, ByteOrder::Little).expect("overrun");
        assert!(overrun.get(0), "the lost intermediate values are flagged");
    }

    #[test]
    fn value_overrun_paths_lists_struct_top_fields() {
        let mut s = epics_pva_rs::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields.push((
            "value".to_string(),
            PvField::Scalar(ScalarValue::Double(1.0)),
        ));
        s.fields
            .push(("alarm".to_string(), PvField::Scalar(ScalarValue::Int(0))));
        let paths = value_overrun_paths(&PvField::Structure(s));
        assert_eq!(paths, vec!["value".to_string(), "alarm".to_string()]);

        // A non-structure value reports no overrun (no gateway PV produces one).
        assert!(value_overrun_paths(&PvField::Scalar(ScalarValue::Double(1.0))).is_empty());
    }

    /// Cooked/typed fanout loss accounting: when a subscriber's broadcast
    /// receiver lags (its single-slot mpsc backpressure stalls the forwarder
    /// while the upstream keeps producing), the next delivered `MonitorUpdate`
    /// must carry the lost leaves in its `overrun` set (pva2pva
    /// moncache.cpp:160-168). Server-side accounting: the cooked DATA frame's
    /// trailing overrun bitset stays hard-empty, as pvxs's is
    /// (servermon.cpp:174-176) — only the raw forwarder puts overrun bits on
    /// the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooked_fanout_reframes_the_snapshot_on_subscriber_lag() {
        let mut src = make_source();
        // Single-slot subscriber queue: the forwarder buffers one update then
        // blocks on the second send, so it cannot drain the broadcast and a
        // burst overflows the entry's broadcast buffer deterministically.
        src.subscriber_queue = 1;
        src.cache.insert_test_entry("OV:PV").await;

        let (_seed, mut rx) = src
            .subscribe_inner(src.cache.clone(), "OV:PV", None)
            .await
            .expect("subscribe to parked test entry");

        // Burst far past the broadcast buffer capacity BEFORE draining, so the
        // stalled forwarder lags and drops intermediate updates. Each value is
        // FOLDED into the entry, so `latest` is the merged snapshot a live
        // upstream would leave behind — 63.0, the value of the last event, most
        // of which this subscriber never saw.
        let mk = |v: f64| {
            let mut s = epics_pva_rs::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".to_string(), PvField::Scalar(ScalarValue::Double(v))));
            PvField::Structure(s)
        };
        for i in 0..64 {
            src.cache.test_publish("OV:PV", mk(i as f64)).await;
        }

        // R19-44: the post-lag update is a RE-FRAME of the merged snapshot —
        // whole structure (`marked: None`), carrying the value of an event the
        // subscriber never received — and it names the lost leaves in `overrun`.
        // Forwarding the next delta instead would have shipped only that
        // event's own leaves, losing every transition inside the dropped range.
        let val = |u: &epics_pva_rs::server_native::MonitorUpdate| match &u.value {
            PvField::Structure(s) => match &s.fields[0].1 {
                PvField::Scalar(ScalarValue::Double(v)) => *v,
                other => panic!("unexpected value shape: {other:?}"),
            },
            other => panic!("unexpected value shape: {other:?}"),
        };
        let mut last_before = None;
        let mut reframed = None;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(u)) if !u.overrun.is_empty() => {
                    reframed = Some(u);
                    break;
                }
                Ok(Some(u)) => {
                    last_before = Some(val(&u));
                    continue;
                }
                Ok(None) | Err(_) => break,
            }
        }
        let u = reframed.expect("a post-lag cooked update must be delivered");
        assert!(
            u.overrun.iter().any(|p| p == "value"),
            "overrun must name the lost leaf, got {:?}",
            u.overrun
        );
        assert!(
            u.marked.is_none(),
            "a re-frame declares the WHOLE structure, not the last delta's leaves"
        );
        // The forwarder may lag before OR after delivering the first event, so
        // the baseline is "the newest value this subscriber could have seen" —
        // 0.0 when the lag beat every delivery. A lag means the 4-slot broadcast
        // overflowed, so the merged snapshot is several events ahead of it
        // either way.
        let before = last_before.unwrap_or(0.0);
        assert!(
            val(&u) > before + 1.0,
            "the re-frame must carry the snapshot AS OF THE LAG — a value from an event this \
             subscriber never received (last seen {before}, re-framed {})",
            val(&u)
        );
    }
}
