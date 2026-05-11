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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use epics_base_rs::server::access_security::{AccessLevel, AccessSecurityConfig};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server::native_source::AcfCell;
use epics_pva_rs::server_native::source::{AccessChecked, ChannelContext, ChannelSource};

use super::channel_cache::ChannelCache;

/// F-G12: raw upstream MONITOR DATA body bytes flowing through the
/// per-entry broadcast channel. `body` is the wire-format
/// `changed | value | overrun` triplet refcount-shared via `Bytes`.
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub body: bytes::Bytes,
    pub byte_order: epics_pva_rs::proto::ByteOrder,
}

/// PV-name → ASG-name resolver. Returns the ASG that the gateway
/// should consult for the given downstream channel. Default impl
/// (see `default_asg_resolver`) returns `"DEFAULT"` for every name —
/// matching the legacy pre-Round-30D behaviour. Sites that want
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
    /// Per-(account, method) upstream PvaClient pool (PG-G10). When
    /// the downstream peer authenticates as `(alice, ca)` the
    /// gateway reuses (or builds) a client whose CONNECTION_VALIDATION
    /// to upstream advertises that same identity, so upstream ASG
    /// rules and audit logs see the *real* client identity, not
    /// the gateway. Empty string keys (`("", "anonymous")`) reuse
    /// the cache's shared client.
    upstream_pool: Arc<Mutex<HashMap<(String, String), Arc<PvaClient>>>>,
    /// Optional gateway-side ACF policy (round 29). When set, every
    /// downstream GET / PUT / MONITOR is gated through
    /// `check_access_method` BEFORE the upstream forward, so the
    /// gateway can deny clients that the upstream IOC would also
    /// deny (or apply site-local policy on top of the upstream
    /// rules). Wrapped in an AcfCell so policy may be hot-swapped
    /// at runtime via `set_acf`. None means pass-through (legacy
    /// pre-round-29 behaviour) — pvxs `pva2pva` parity for sites
    /// that delegate all ACL to upstream.
    acf: AcfCell,
    /// PV→ASG resolver. Defaults to `DEFAULT` for every name; sites
    /// that want per-PV ASG granularity replace this via
    /// [`set_asg_resolver`].
    ///
    /// Round-32D (R31-G8): wrapped in `RwLock` so the resolver can
    /// be hot-swapped at runtime — the gateway is typically handed
    /// off to `PvaServer` behind an `Arc` (or its trait-object
    /// equivalent), so a `&mut self` setter is unreachable after
    /// installation. Mirrors the `acf` cell's hot-swap pattern.
    asg_resolver: Arc<RwLock<AsgResolver>>,
    /// Round 41: type-state-enforced access gate. The closure
    /// captures the `asg_resolver` cell so a hot-swap of the
    /// resolver via [`set_asg_resolver`] is visible on the next
    /// `check`. ASL is fixed at 0 for gateway-side checks — the
    /// upstream record's ASL is not visible to the gateway and the
    /// site policy on the gateway is expected to use UAG/HAG
    /// gating rather than per-record ASL.
    gate: epics_base_rs::server::access_security::AccessGate,
}

impl GatewayChannelSource {
    pub fn new(cache: Arc<ChannelCache>) -> Self {
        let acf: AcfCell = Arc::new(RwLock::new(None));
        let asg_resolver = Arc::new(RwLock::new(default_asg_resolver()));
        let gate = Self::build_gate(acf.clone(), asg_resolver.clone());
        Self {
            cache,
            connect_timeout: Duration::from_secs(5),
            subscriber_queue: 64,
            rpc_timeout: Duration::from_secs(30),
            max_subscribers: 100_000,
            subscriber_count: Arc::new(AtomicUsize::new(0)),
            upstream_pool: Arc::new(Mutex::new(HashMap::new())),
            acf,
            asg_resolver,
            gate,
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
    /// Round-32D: takes `&self` and writes through `RwLock`, so the
    /// resolver may be replaced after the source has been handed to
    /// `PvaServer` behind an `Arc`.
    pub async fn set_asg_resolver(&self, resolver: Option<AsgResolver>) {
        *self.asg_resolver.write().await = resolver.unwrap_or_else(default_asg_resolver);
        // R49-G2: bump the gate's ACL generation so monitor tasks
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
        // R49-G2: bump the gate's ACL generation — see comment on
        // `set_asg_resolver`.
        self.gate.bump_acl_version();
    }

    // Round 49 follow-up: the `acf_cell()` accessor was removed. It
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
    /// Round 49 follow-up: gated on `#[cfg(test)]` so production
    /// code physically cannot bypass the gate by calling this
    /// alternate ACL path — the single owner of an ACL evaluation
    /// in a non-test build is `self.gate`.
    #[cfg(test)]
    async fn acl_level(&self, pv: &str, ctx: &ChannelContext) -> AccessLevel {
        let guard = self.acf.read().await;
        match *guard {
            None => AccessLevel::ReadWrite,
            Some(ref cfg) => {
                // Round-32D: read-lock the resolver cell; the closure
                // runs while the read lock is held so the swap is
                // serialized vs. in-flight evaluations. The closure
                // itself should be O(1) — comment on `AsgResolver`
                // documents this.
                let resolver = self.asg_resolver.read().await;
                let asg = (resolver)(pv);
                cfg.check_access_method(
                    &asg,
                    &ctx.host,
                    &ctx.account,
                    0,
                    &ctx.method,
                    "",
                )
            }
        }
    }

    /// Look up (or lazily build) the upstream client for the given
    /// downstream credentials. PG-G10: each unique (account, method)
    /// pair gets its own connection so upstream ASG rules see the
    /// real client identity. Empty/anonymous credentials fall through
    /// to the cache's shared client (no new connection allocated).
    fn upstream_client_for(&self, ctx: &ChannelContext) -> Arc<PvaClient> {
        if ctx.account.is_empty() || ctx.method == "anonymous" {
            return self.cache.client().clone();
        }
        let key = (ctx.account.clone(), ctx.method.clone());
        let mut pool = self.upstream_pool.lock();
        if let Some(c) = pool.get(&key) {
            return c.clone();
        }
        let client = Arc::new(
            PvaClient::builder()
                .user(ctx.account.clone())
                .host(ctx.host.clone())
                .build(),
        );
        pool.insert(key, client.clone());
        client
    }

    /// Cache handle — useful for the gateway's own diagnostics.
    pub fn cache(&self) -> &Arc<ChannelCache> {
        &self.cache
    }

    /// Diagnostic accessor: how many entries are currently cached.
    pub async fn cached_entry_count(&self) -> usize {
        self.cache.entry_count().await
    }

    /// Diagnostic: live subscribe-bridge tasks.
    pub fn live_subscribers(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
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

    async fn get_value(&self, name: &str) -> Option<PvField> {
        let entry = self.cache.lookup(name, self.connect_timeout).await.ok()?;
        // Prefer the cached monitor snapshot — same value the upstream
        // server would return to a fresh GET, no extra round-trip.
        entry.snapshot()
    }

    /// Round-29: gateway-side ACF gate for GET. Denies with `None`
    /// (wire layer surfaces ECA_NORDACCESS-equivalent) before the
    /// upstream lookup, so a denied client never opens an upstream
    /// channel and never appears in upstream audit logs as the
    /// gateway.
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), String> {
        // Look up the entry to keep the upstream channel alive (and
        // confirm the PV exists) before issuing the PUT through the
        // shared client. The client's connection pool reuses the
        // already-open server connection.
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| e.to_string())?;
        let value_str = pvfield_to_pvput_string(&value)
            .ok_or_else(|| "unsupported PvField shape for upstream PUT".to_string())?;
        self.cache
            .client()
            .pvput(name, &value_str)
            .await
            .map_err(|e| e.to_string())
    }

    /// Credential-aware PUT (PG-G10) — Round 43 migration to the
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
    ) -> Result<(), String> {
        if !checked.allows_write() {
            tracing::debug!(
                pv = %checked.pv_name(),
                account = %ctx.account,
                method = %ctx.method,
                "pva-gateway: PUT denied by gateway ACF"
            );
            return Err(format!(
                "PUT denied by gateway access security: \
                 PV '{pv}' from {host}/{account}/{method}",
                pv = checked.pv_name(),
                host = ctx.host,
                account = ctx.account,
                method = ctx.method,
            ));
        }

        let name = checked.pv_name();
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| e.to_string())?;
        let value_str = pvfield_to_pvput_string(&value)
            .ok_or_else(|| "unsupported PvField shape for upstream PUT".to_string())?;
        let client = self.upstream_client_for(&ctx);
        tracing::debug!(
            pv = %name,
            account = %ctx.account,
            method = %ctx.method,
            "pva-gateway: forwarding PUT with downstream credentials"
        );
        client
            .pvput(name, &value_str)
            .await
            .map_err(|e| e.to_string())
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
    ) -> Result<(FieldDesc, PvField), String> {
        let _entry = self
            .cache
            .lookup(name, self.connect_timeout)
            .await
            .map_err(|e| e.to_string())?;
        let result = tokio::time::timeout(
            self.rpc_timeout,
            self.cache
                .client()
                .pvrpc(name, &request_desc, &request_value),
        )
        .await;
        match result {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("upstream rpc timeout for {name}")),
        }
    }

    async fn subscribe_raw(
        &self,
        name: &str,
    ) -> Option<mpsc::Receiver<epics_pva_rs::server_native::RawMonitorEvent>> {
        // F-G12 fast path: hand the server pre-encoded raw bodies so
        // its dispatch can write them onto downstream sockets without
        // re-running encode_pv_field. The cache spawns the upstream
        // monitor task (one decode per upstream event) and broadcasts
        // the encoded body to N receivers — N atomic refcount bumps,
        // not N encodes.
        //
        // F-G12 default ON — raw forwarding is the production
        // gateway path. Operators can opt out via
        // `EPICS_PVA_GW_RAW_FRAMES=NO` if they hit a regression and
        // want the legacy decode-then-encode path while issues are
        // diagnosed.
        if let Some(v) = epics_base_rs::runtime::env::get("EPICS_PVA_GW_RAW_FRAMES") {
            if v.eq_ignore_ascii_case("NO") || v.eq_ignore_ascii_case("FALSE") || v == "0" {
                return None;
            }
        }
        // R49-G4: bump the gateway-wide subscriber count BEFORE
        // spawning the forwarder. Pre-fix the raw path skipped the
        // increment but the spawned CounterGuard's Drop still
        // performed the decrement, underflowing the counter on
        // every raw subscription teardown; subsequent decoded
        // subscribes would then read the wrapped-around `usize` and
        // refuse new subscribers under a false "cap reached"
        // warning. Mirror the decoded subscribe's cap check + RAII
        // decrement here too so raw subscriptions count against
        // the same ceiling.
        let prev = self
            .subscriber_count
            .fetch_add(1, Ordering::Relaxed);
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
        let entry = match self.cache.lookup(name, self.connect_timeout).await {
            Ok(e) => e,
            Err(_) => {
                self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut bcast = entry.subscribe_raw();
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
            loop {
                match bcast.recv().await {
                    Ok(ev) => {
                        let out = epics_pva_rs::server_native::RawMonitorEvent {
                            body_bytes: ev.body,
                            byte_order: ev.byte_order,
                        };
                        if mpsc_tx.send(out).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Some(mpsc_rx)
    }

    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
        // Gateway-wide subscriber cap (PG-G3). The underlying
        // PvaServer enforces a per-connection channel cap; this is
        // the global ceiling that defends against a coordinated
        // burst of N peers each requesting M monitors.
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

        let entry = match self.cache.lookup(name, self.connect_timeout).await {
            Ok(e) => e,
            Err(_) => {
                self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut bcast_rx = entry.subscribe();
        // pvxs sends one event per subscribe so the downstream sees
        // the current value immediately; emit our cached snapshot the
        // same way.
        let initial = entry.snapshot();

        let (mpsc_tx, mpsc_rx) = mpsc::channel(self.subscriber_queue);
        let counter = self.subscriber_count.clone();
        tokio::spawn(async move {
            // RAII: ensure the counter is always decremented even on
            // panic / early-return paths.
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Slow consumer; broadcast dropped some
                        // events. Swallow and keep going — next event
                        // resyncs the cache.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Some(mpsc_rx)
    }

    /// Forward downstream-to-gateway backpressure into upstream
    /// pipeline pause. The PvaServer fires this when a per-connection
    /// monitor outbox crosses the high watermark (downstream peer not
    /// draining fast enough). PG-G9: we now look up the per-PV
    /// `Pauser` (installed by the auto-restart task in
    /// `channel_cache.rs::spawn_upstream_monitor`) and spawn a task
    /// to send the `MonitorPause` control to the upstream — pvxs
    /// `MonitorControlOp::pipeline` parity. Best effort: if the
    /// entry isn't currently connected to upstream we just log.
    fn notify_watermark_high(&self, name: &str) {
        tracing::warn!(
            pv = %name,
            "pva-gateway: downstream monitor outbox crossed high watermark"
        );
        // Synchronous lookup via Mutex (no .await) — peek doesn't
        // exist sync, but `entries` lives in tokio::sync::Mutex
        // which only has async lock. Spawn a task that does the
        // async pause; this trait method is sync.
        let cache = self.cache.clone();
        let name_owned = name.to_string();
        tokio::spawn(async move {
            if let Some(entry) = cache.peek(&name_owned).await {
                if let Some(p) = entry.pauser_snapshot() {
                    p.pause().await;
                }
            }
        });
    }

    fn notify_watermark_low(&self, name: &str) {
        tracing::debug!(
            pv = %name,
            "pva-gateway: downstream monitor outbox drained below low watermark"
        );
        let cache = self.cache.clone();
        let name_owned = name.to_string();
        tokio::spawn(async move {
            if let Some(entry) = cache.peek(&name_owned).await {
                if let Some(p) = entry.pauser_snapshot() {
                    p.resume().await;
                }
            }
        });
    }
}

/// Convert a `PvField` into the string form pvput accepts. Covers:
/// * `Scalar` / `ScalarArray` directly
/// * `Structure` containing a `.value` field (NTScalar / NTScalarArray /
///   NTEnum index — anything where the put target is the canonical
///   `value` subfield)
/// * `Variant` and `Union` by recursively unwrapping the inner field
///
/// Returns `None` for shapes pvput cannot represent in string form
/// (e.g. nested structures with no `value` field). Callers surface
/// the `None` to the downstream client as a typed error so the user
/// gets a clear "unsupported PvField shape" message instead of a
/// silent drop. Without `pvput_field` (typed PUT through the client
/// API) on `PvaClient` this is the best the gateway can do today;
/// see review §3d for the longer-term plan.
fn pvfield_to_pvput_string(v: &PvField) -> Option<String> {
    match v {
        PvField::Scalar(sv) => Some(scalar_to_string(sv)),
        PvField::ScalarArray(items) => {
            // pvput accepts space-separated values for arrays.
            let parts: Vec<String> = items.iter().map(scalar_to_string).collect();
            Some(parts.join(" "))
        }
        PvField::Structure(s) => {
            for (name, field) in &s.fields {
                if name == "value" {
                    return pvfield_to_pvput_string(field);
                }
            }
            None
        }
        PvField::Variant(boxed) => pvfield_to_pvput_string(&boxed.value),
        PvField::Union {
            selector, value, ..
        } => {
            if *selector < 0 {
                None
            } else {
                pvfield_to_pvput_string(value)
            }
        }
        _ => None,
    }
}

fn scalar_to_string(sv: &epics_pva_rs::pvdata::ScalarValue) -> String {
    use epics_pva_rs::pvdata::ScalarValue::*;
    match sv {
        Boolean(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Byte(x) => x.to_string(),
        UByte(x) => x.to_string(),
        Short(x) => x.to_string(),
        UShort(x) => x.to_string(),
        Int(x) => x.to_string(),
        UInt(x) => x.to_string(),
        Long(x) => x.to_string(),
        ULong(x) => x.to_string(),
        Float(x) => x.to_string(),
        Double(x) => x.to_string(),
        String(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::access_security::parse_acf;

    fn make_ctx(host: &str, account: &str, method: &str) -> ChannelContext {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            account: account.to_string(),
            method: method.to_string(),
            host: host.to_string(),
        }
    }

    fn make_source() -> GatewayChannelSource {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(client, Duration::from_secs(60));
        GatewayChannelSource::new(cache)
    }

    /// Round-29 baseline: with no ACF attached, `acl_level` reports
    /// `ReadWrite` so the gateway's pre-existing pass-through fast
    /// path stays hot. Pre-round-29 was the only behaviour.
    #[tokio::test]
    async fn acl_level_no_acf_is_readwrite() {
        let src = make_source();
        let level = src.acl_level("any:pv", &make_ctx("h", "anyone", "anonymous")).await;
        assert!(matches!(level, AccessLevel::ReadWrite));
    }

    /// Round-29: with an ACF attached, downstream peers must match
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
        let err = src
            .put_value_ctx("any:pv", dummy_value, make_ctx("h", "intruder", "anonymous"))
            .await
            .expect_err("PUT must be denied for non-admin under DEFAULT ASG");
        assert!(
            err.contains("denied by gateway access security"),
            "denial reason must name the gateway as enforcement point: {err:?}",
        );
    }

    /// Round-29: a NoAccess rule denies GET and MONITOR (returns
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
        assert!(
            src.get_value_ctx("any:pv", ctx.clone()).await.is_none(),
            "GET must be denied for non-ops"
        );
        assert!(
            src.subscribe_ctx("any:pv", ctx).await.is_none(),
            "MONITOR must be denied for non-ops"
        );
    }

    /// Round-29: hot-swapping the ACF cell takes effect on the next
    /// op. Mirrors the round-28 PVA-server-side AcfCell test.
    #[tokio::test]
    async fn acf_swap_takes_effect_on_next_op() {
        let src = make_source();
        let deny = parse_acf(r#"ASG(DEFAULT) { RULE(1, READ) }"#).unwrap();
        src.set_acf(Some(deny)).await;

        let dummy_value = PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(0.0));
        let ctx = make_ctx("h", "anyone", "anonymous");
        assert!(
            src.put_value_ctx("any:pv", dummy_value.clone(), ctx.clone())
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
        let result = src
            .put_value_ctx("any:pv", dummy_value, ctx)
            .await;
        if let Err(msg) = result {
            assert!(
                !msg.contains("denied by gateway access security"),
                "post-swap PUT must NOT be ACL-denied: {msg:?}",
            );
        }
    }

    /// Round-30D: the gateway no longer hard-codes the ASG name to
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
        }))).await;

        let guest = make_ctx("anyhost", "guest", "anonymous");
        let admin = make_ctx("anyhost", "admin", "anonymous");

        // DEFAULT-routed PV: open to everyone for RW.
        assert_eq!(src.acl_level("other:val", &guest).await, AccessLevel::ReadWrite);

        // OPERATOR-routed PV: guest READ-only, admin RW.
        assert_eq!(src.acl_level("set:current", &guest).await, AccessLevel::Read);
        assert_eq!(src.acl_level("set:current", &admin).await, AccessLevel::ReadWrite);

        // LOCKED-routed PV: everyone READ-only (no WRITE rule).
        assert_eq!(src.acl_level("dev:hwid", &admin).await, AccessLevel::Read);
        assert_eq!(src.acl_level("dev:hwid", &guest).await, AccessLevel::Read);
    }

    /// Round-32D (R31-G8): the resolver must be hot-swappable after
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
        src.set_asg_resolver(Some(Arc::new(|_pv| "LOCKED".to_string()))).await;
        assert_eq!(src.acl_level("X", &ctx).await, AccessLevel::Read);

        // Swap back to default.
        src.set_asg_resolver(None).await;
        assert_eq!(src.acl_level("X", &ctx).await, AccessLevel::ReadWrite);
    }

    /// Round-37 (R37-G1): RPC must consult the gateway ACF too.
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

        let result = src
            .rpc_ctx(
                "some:rpc",
                FieldDesc::Variant,
                PvField::Null,
                make_ctx("anyhost", "intruder", "anonymous"),
            )
            .await;
        let err = result.expect_err("RPC must be denied for NoAccess peer");
        assert!(
            err.contains("denied by gateway access security"),
            "denial message must name the gateway as enforcer: {err:?}",
        );
    }

    /// Round-32A (R31-G6): the F-G12 raw-frame fast path must consult
    /// the same ACF gate as the decoded `subscribe_ctx` path. Pre-fix
    /// a NoAccess peer could mount a `subscribe_raw` subscription
    /// (since the round-29 gate covered `subscribe_ctx` only) and
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

        let rx = src
            .subscribe_raw_ctx(
                "any:pv",
                make_ctx("anyhost", "intruder", "anonymous"),
            )
            .await;
        assert!(
            rx.is_none(),
            "raw subscribe must be denied for a NoAccess peer"
        );
    }
}
