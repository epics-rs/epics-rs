//! Upstream CA client adapter for the gateway.
//!
//! Manages connections to upstream IOCs via [`epics_ca_rs::client::CaClient`].
//! When a downstream client first searches for a PV, the gateway uses
//! `UpstreamManager` to:
//!
//! 1. Create a CA channel to the upstream IOC
//! 2. Subscribe with a monitor + a one-shot GET to learn the native type
//! 3. Spawn a task that forwards events into the [`PvCache`] and a
//!    shadow [`PvDatabase`] (which the downstream CaServer queries)
//! 4. Install a [`WriteHook`] on the shadow PV so client-originated
//!    writes are forwarded upstream rather than landing locally
//!
//! When the last downstream subscriber leaves, the upstream channel is
//! kept alive (Inactive state) until the cache cleanup timer evicts it.
//!
//! ## Shadow PvDatabase pattern
//!
//! The gateway uses two stores in parallel:
//!
//! - [`PvCache`] — gateway's view (state machine, subscriber list, stats)
//! - [`PvDatabase`] — `epics-ca-rs::server::CaServer`'s view (the actual
//!   PVs that downstream clients see)
//!
//! Both are kept in sync: every upstream event updates `PvCache.cached`
//! AND posts to the shadow `PvDatabase` via `put_pv_and_post()`, which
//! triggers the CaServer to fan out monitor events to all attached
//! downstream clients.
//!
//! ## Auto-restart
//!
//! The monitor-forwarding task wraps `channel.subscribe()` in an
//! exponential-backoff retry loop so a transient upstream disconnect
//! does not strand the cache entry forever (the entry's `cached`
//! snapshot would otherwise be served indefinitely while no further
//! events arrive). On terminal failure the entry transitions to the
//! `Disconnect` state, which the cleanup tick eventually evicts.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{WriteContext, WriteHook};
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, EventWatcher};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::error::{BridgeError, BridgeResult};

use super::access::AccessConfig;
use super::cache::{PvCache, PvState};
use super::putlog::{PutLog, PutOutcome};
use super::pvlist::PvList;
use super::stats::Stats;

/// The `.pvlist` access-security identity (group + level) resolved for
/// one shadow PV. Held behind an `Arc<ArcSwap<_>>` ([`PvAclCell`]) so it
/// is *mutable per-shadow-PV gateway metadata* rather than a value
/// captured permanently in the read/write hook closures: a SIGUSR1
/// `AS`/`PVL` reload that moves a still-admitted PV from one ASG/ASL to
/// another swaps in the new identity live, and both hooks `load()` it on
/// every access. Mirrors C ca-gateway `gateServer::newAs` reinstalling
/// the freshly-resolved `gateAsEntry` on each still-allowed PV
/// (gateServer.cc:603-630) instead of leaving the old entry bound.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PvAcl {
    /// Resolved access security group from the matching `.pvlist` rule
    /// (`None` ⇒ the `DEFAULT` group, applied at the hook read site).
    asg: Option<String>,
    /// Resolved access security level (paired with `asg`).
    asl: i32,
}

/// Shared, hot-swappable per-PV ACL. The subscription record owns one
/// `Arc`; the read/write hook closures each capture a clone and `load()`
/// the current value, so a reload that calls [`UpstreamManager::update_acl`]
/// is observed by both hooks without rebuilding them.
type PvAclCell = Arc<ArcSwap<PvAcl>>;

/// One upstream subscription: the long-lived [`CaChannel`] is shared
/// between the monitor-forwarding task and any direct
/// [`UpstreamManager::put`] / [`UpstreamManager::get`] calls so we
/// don't pay a fresh CREATE_CHAN round-trip per write. Mirrors C
/// CA-gateway behaviour (cas/io/casChannel.cc) where the upstream
/// channel is reused across the gateway's lifetime.
struct UpstreamSubscription {
    channel: Arc<CaChannel>,
    task: JoinHandle<()>,
    /// Live `.pvlist` ASG/ASL for this shadow PV, shared by-`Arc` with
    /// the read and write hook closures so a `AS`/`PVL` reload can swap
    /// the group/level in place. Replaces the previous by-value
    /// `asg`/`asl` capture that left hooks enforcing the identity
    /// resolved at first subscription forever.
    acl: PvAclCell,
    /// watcher that keeps `upstream_write` in the access hook
    /// closure up-to-date. Aborted on drop (when the subscription is
    /// removed via `unsubscribe`).
    _access_rights_watcher: EventWatcher,
}

/// Shared state every upstream subscription's WriteHook needs. Hoisted
/// out of `UpstreamManager` so the per-subscription closure can capture
/// a single small `Arc` instead of a long list of separate handles.
///
/// `access` and `pvlist` are wrapped in `ArcSwap` for lock-free reads
/// on the put hot-path: previously each put took two sequential
/// `tokio::RwLock::read().await.clone()` calls, which serialized
/// against SIGUSR1 reload writes (and against each other under high
/// contention). `ArcSwap::load_full()` is wait-free.
#[derive(Clone)]
struct WriteHookEnv {
    /// Read-only mode rejects all puts.
    read_only: bool,
    /// Live access security config (hot-reloadable, lock-free reads).
    access: Arc<ArcSwap<AccessConfig>>,
    /// Live pvlist (hot-reloadable). Used for `DENY FROM host` checks.
    pvlist: Arc<ArcSwap<PvList>>,
    /// Optional put-event log.
    putlog: Option<Arc<PutLog>>,
    /// Gateway PV cache, keyed by `served_name`. The write hook reads
    /// `GwPvEntry.cached` (the last upstream monitor value) for the
    /// put-audit `old=` field — the analog of C ca-gateway's
    /// `vc->eventData()`. Only read when `putlog` is configured.
    cache: Arc<RwLock<PvCache>>,
    /// Stats counters.
    stats: Arc<Stats>,
    /// Beacon-anomaly trigger handle. Fires from
    /// `ensure_subscribed`'s first-Active transition (so other gateway-
    /// aware downstream clients re-search and discover this gateway as
    /// the server for the just-added PV) and from upstream-reconnect
    /// transitions in the forwarding task. Mirrors C++ ca-gateway's
    /// `gateServer::generateBeaconAnomaly`.
    beacon_anomaly: Arc<super::beacon::BeaconAnomaly>,
}

/// Configuration handed to [`UpstreamManager::new`]. Groups the
/// 7-arg parameter list into one struct so the next caller doesn't
/// accidentally swap `pvlist` and `access` (same type) and so adding
/// a new policy field (e.g. monitor watermark) doesn't cascade into
/// every call site.
pub struct UpstreamManagerConfig {
    pub cache: Arc<RwLock<PvCache>>,
    pub shadow_db: Arc<PvDatabase>,
    pub access: Arc<ArcSwap<AccessConfig>>,
    pub pvlist: Arc<ArcSwap<PvList>>,
    pub putlog: Option<Arc<PutLog>>,
    pub stats: Arc<Stats>,
    pub read_only: bool,
    /// Upstream connect-timeout budget for lazy search resolution.
    /// Threaded from `GatewayConfig::timeouts.connect_timeout` so the
    /// CLI/API connect-timeout knob also governs first-search resolution,
    /// not just cache cleanup (a single connect-timeout owner).
    pub connect_timeout: Duration,
    pub beacon_anomaly: Arc<super::beacon::BeaconAnomaly>,
    /// B10: optional TLS client config for the upstream `CaClient`
    /// circuits to the real IOC. `None` keeps upstream traffic
    /// plaintext (the default). Independent of downstream TLS
    /// termination. Available with the `ca-gateway-tls` feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub upstream_tls: Option<epics_ca_rs::tls::TlsConfig>,
    /// B10: SNI / cert-hostname-verification override for the
    /// upstream TLS connections. Available with the `ca-gateway-tls`
    /// feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub upstream_tls_server_name: Option<String>,
}

/// Manages upstream CA client connections for the gateway.
///
/// Holds a single shared `CaClient` and tracks per-PV channel +
/// monitor task pairs. The channel is reused for PUT / GET so the
/// gateway does not re-do CA handshake on every write.
pub struct UpstreamManager {
    client: Arc<CaClient>,
    cache: Arc<RwLock<PvCache>>,
    shadow_db: Arc<PvDatabase>,
    /// Shared environment captured by every per-PV WriteHook closure.
    write_env: WriteHookEnv,
    /// Active upstream subscriptions, keyed by PV name. Holding the
    /// channel here keeps it alive for the gateway's lifetime so
    /// every PUT / GET reuses one circuit. Wrapped in parking_lot so
    /// `ensure_subscribed` can take `&self` and the search-resolver
    /// hot path doesn't serialise N concurrent first-creates behind
    /// a single tokio RwLock write held across ~500 ms of network IO.
    subs: parking_lot::Mutex<HashMap<String, UpstreamSubscription>>,
    /// In-flight first-create dedupe. A second concurrent caller for
    /// the same PV awaits the Notify instead of duplicating the
    /// upstream channel + subscribe + spawn work.
    pending: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    /// Budget for the lazy-resolution `wait_connected` gate in
    /// `ensure_subscribed`. Sourced from `CacheTimeouts::connect_timeout`
    /// so the connect-timeout policy has ONE owner shared with the cache
    /// reaper — mirrors C ca-gateway routing every pending-connect budget
    /// through the single `global_resources->connectTimeout()`
    /// (gateServer.cc:1294-1356, set once via setConnectTimeout,
    /// gateway.cc:1147) rather than a second hard-coded clock.
    connect_timeout: Duration,
}

impl UpstreamManager {
    /// Create a new upstream manager from the grouped config.
    ///
    /// `cache` is the gateway's PV cache. `shadow_db` is the
    /// `PvDatabase` the downstream `CaServer` queries. The remaining
    /// handles (`access`, `pvlist`, `putlog`, `stats`, `read_only`) are
    /// captured by every PV's WriteHook so client-originated puts
    /// enforce the gateway's full policy (read-only, ACL, host deny,
    /// putlog) before forwarding upstream.
    pub async fn new(cfg: UpstreamManagerConfig) -> BridgeResult<Self> {
        // B10: when an upstream TLS config is supplied, build the
        // `CaClient` with it so every TCP virtual circuit to the real
        // IOC is wrapped in TLS. `CaClient::new()` would instead pick
        // TLS up from the `EPICS_CA_TLS_*` env vars — explicit config
        // wins, so the gateway can run a TLS upstream policy distinct
        // from whatever the ambient environment specifies. Without an
        // upstream-TLS config we keep `CaClient::new()` so the env-var
        // path still works for operators who prefer it.
        #[cfg(feature = "ca-gateway-tls")]
        let client = if cfg.upstream_tls.is_some() || cfg.upstream_tls_server_name.is_some() {
            let mut client_cfg = epics_ca_rs::client::CaClientConfig::default();
            client_cfg.tls = cfg.upstream_tls.clone();
            client_cfg.tls_server_name = cfg.upstream_tls_server_name.clone();
            CaClient::new_with_config(client_cfg)
                .await
                .map_err(|e| BridgeError::PutRejected(format!("CaClient init (TLS): {e}")))?
        } else {
            CaClient::new()
                .await
                .map_err(|e| BridgeError::PutRejected(format!("CaClient init: {e}")))?
        };
        #[cfg(not(feature = "ca-gateway-tls"))]
        let client = CaClient::new()
            .await
            .map_err(|e| BridgeError::PutRejected(format!("CaClient init: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
            cache: cfg.cache.clone(),
            shadow_db: cfg.shadow_db,
            write_env: WriteHookEnv {
                read_only: cfg.read_only,
                access: cfg.access,
                pvlist: cfg.pvlist,
                putlog: cfg.putlog,
                cache: cfg.cache,
                stats: cfg.stats,
                beacon_anomaly: cfg.beacon_anomaly,
            },
            subs: parking_lot::Mutex::new(HashMap::new()),
            pending: parking_lot::Mutex::new(HashMap::new()),
            connect_timeout: cfg.connect_timeout,
        })
    }

    /// Number of active upstream subscriptions.
    pub fn subscription_count(&self) -> usize {
        self.subs.lock().len()
    }

    /// Whether a given upstream name is currently subscribed.
    pub fn is_subscribed(&self, name: &str) -> bool {
        self.subs.lock().contains_key(name)
    }

    /// Ensure an upstream subscription exists for `upstream_name`.
    ///
    /// If already subscribed, this is a no-op. Otherwise:
    /// 1. Create CA channel to upstream
    /// 2. Try a one-shot `get()` so the shadow PV is registered with
    ///    the upstream's *native* DBR type rather than `Double(0.0)`
    ///    placeholder — improves first-read fidelity. Falls back to
    ///    `Double(0.0)` if the get fails or times out.
    /// 3. Subscribe (monitor)
    /// 4. Insert/update cache entry to `Connecting`
    /// 5. Spawn forwarding task with auto-restart
    /// 6. Install per-PV WriteHook on the shadow PV
    ///
    /// `served_name` is the downstream-facing identity the
    /// client searched for (a `.pvlist` `ALIAS` name, or — for a plain
    /// `ALLOW` match — the same string as `upstream_name`). The shadow
    /// PV, subscription dedup key, cache entry, and monitor fan-out all
    /// key on `served_name` so a client using the alias can complete
    /// `CREATE_CHANNEL`/read. `upstream_name` is the resolved real PV the
    /// gateway connects to upstream; the CA channel, write hook (put
    /// forwarding + putlog), and DBR negotiation get all target it.
    /// Mirrors C ca-gateway attaching the requested alias to the real
    /// `gateVcData` (`gateServer.cc:1747`).
    pub async fn ensure_subscribed(
        &self,
        served_name: &str,
        upstream_name: &str,
        asg: Option<String>,
        asl: i32,
    ) -> BridgeResult<()> {
        // Fast path: already subscribed.
        if self.subs.lock().contains_key(served_name) {
            return Ok(());
        }
        // Concurrent first-create dedupe. If another task is already
        // wiring this PV up, await its completion instead of running
        // the create_channel/get/subscribe work twice. The Notify is
        // pulsed in the success and error paths below.
        enum Decision {
            WaitFor(Arc<tokio::sync::Notify>),
            Owner(Arc<tokio::sync::Notify>),
        }
        let decision = {
            let mut pending = self.pending.lock();
            if let Some(existing) = pending.get(served_name) {
                Decision::WaitFor(existing.clone())
            } else {
                let n = Arc::new(tokio::sync::Notify::new());
                pending.insert(served_name.to_string(), n.clone());
                Decision::Owner(n)
            }
        };
        let dedup_notify = match decision {
            Decision::WaitFor(n) => {
                n.notified().await;
                let already = self.subs.lock().contains_key(served_name);
                return if already {
                    Ok(())
                } else {
                    Err(BridgeError::PutRejected(format!(
                        "upstream subscribe failed (peer creator): {served_name}"
                    )))
                };
            }
            Decision::Owner(n) => n,
        };
        // Ensure the pending entry is removed + Notify pulsed even
        // on error / panic paths.
        struct PendingGuard<'a> {
            owner: &'a UpstreamManager,
            key: &'a str,
            notify: Arc<tokio::sync::Notify>,
        }
        impl Drop for PendingGuard<'_> {
            fn drop(&mut self) {
                self.owner.pending.lock().remove(self.key);
                self.notify.notify_waiters();
            }
        }
        let _guard = PendingGuard {
            owner: self,
            key: served_name,
            notify: dedup_notify.clone(),
        };

        // Add entry to cache (or get existing) and reset to Connecting
        {
            let mut cache = self.cache.write().await;
            let entry = cache.get_or_create(served_name);
            entry.write().await.set_state(PvState::Connecting);
        }

        // Create one CA channel and reuse it for both monitor and
        // direct PUT/GET. Stored as Arc so the lifecycle guard fires
        // exactly once when the subscription is dropped.
        let channel = Arc::new(self.client.create_channel(upstream_name));

        // only register a shadow PV once the upstream channel
        // actually connects. Without this gate the get() below times out,
        // falls back to Double(0.0), and we register a placeholder the
        // search resolver then advertises as existing — black-holing a
        // name that matches the pvlist pattern but is absent upstream.
        // C ca-gateway answers does-not-exist for an unconnected PV
        // (gatePvData::death → pverDoesNotExistHere, gatePv.cc:622) and
        // exists-here only after connect (life → pverExistsHere,
        // gatePv.cc:518). The connect budget is the configured
        // `CacheTimeouts::connect_timeout`, the SAME value the cache
        // reaper uses to drop stuck Connecting entries (cache.rs:323) —
        // one owner, so a longer CLI/API connect-timeout governs lazy
        // resolution too instead of being overridden by a local constant.
        if let Err(e) = channel.wait_connected(self.connect_timeout).await {
            // Drop the Connecting cache entry created above; no shadow PV
            // is registered, so the resolver answers does-not-exist. The
            // dedup PendingGuard notifies waiters, who then see no live
            // subscription and propagate the same miss.
            self.cache.write().await.remove(served_name);
            tracing::info!(
                pv = upstream_name,
                served = served_name,
                error = %e,
                "ca-gateway-rs: upstream did not connect within connect_timeout; \
                 treating search as a miss"
            );
            return Err(BridgeError::PutRejected(format!(
                "upstream PV did not connect: {upstream_name}"
            )));
        }

        // DBR negotiation: best-effort initial GET so the shadow
        // PV's first registered type matches upstream's native type.
        // Falls back to a Double placeholder if the get fails or
        // times out — the first monitor event will overwrite the
        // value either way. The timeout/error is logged at INFO so
        // an operator chasing type-mismatch confusion can correlate
        // a confused downstream introspect with its upstream miss.
        // channel.get() returns (DbFieldType, EpicsValue)
        // only — no DBR_CTRL_* metadata (units/precision/limits).
        // A DBR_CTRL GET + DBE_PROPERTY subscription is needed so
        // downstream DBR_CTRL_* and DBR_GR_* reads return real
        // metadata (gatePv.cc:930-934, gatePv.cc:858-862).
        let initial_value = match tokio::time::timeout(Duration::from_millis(500), channel.get())
            .await
        {
            Ok(Ok((_dbf, v))) => v,
            Ok(Err(e)) => {
                tracing::info!(
                    pv = upstream_name,
                    error = %e,
                    "ca-gateway-rs: DBR negotiation get failed; using Double(0.0) placeholder"
                );
                EpicsValue::Double(0.0)
            }
            Err(_) => {
                tracing::info!(
                    pv = upstream_name,
                    "ca-gateway-rs: DBR negotiation get timed out; using Double(0.0) placeholder"
                );
                EpicsValue::Double(0.0)
            }
        };

        // read initial upstream write-access and create a flag
        // the access hook AND write-hook closures share. `channel.info()`
        // reads a cached snapshot — no round-trip — and succeeds here
        // because `channel.get()` above already waited for connection.
        // Defaults to true (permissive) on the rare timeout/error path so
        // we match the C gateway's connect-time behaviour (activate() calls
        // ca_write_access only after the channel is operational).
        let upstream_write_init = channel
            .info()
            .await
            .map(|i| i.access_rights.write)
            .unwrap_or(true);
        let upstream_write = Arc::new(AtomicBool::new(upstream_write_init));

        // Atomically register the shadow PV WITH its WriteHook
        // attached. `add_pv_with_hook` constructs the PV with the
        // hook installed before inserting into `simple_pvs`, so a
        // downstream client cannot race a CREATE_CHAN + WRITE_NOTIFY
        // into the small window where the PV is findable but the
        // hook isn't yet bound (which would silently drop the put
        // into the local `pv.set()` fallback).
        // One live ACL cell shared by both hooks AND the subscription
        // record, so an `AS`/`PVL` reload that re-resolves this PV to a
        // new ASG/ASL is observed by every later read/write without
        // rebuilding the closures.
        let acl: PvAclCell = Arc::new(ArcSwap::from_pointee(PvAcl { asg, asl }));
        let hook = build_write_hook(
            served_name.to_string(),
            upstream_name.to_string(),
            channel.clone(),
            acl.clone(),
            self.write_env.clone(),
        );
        // install the read/write access hook alongside the
        // write hook, capturing the same ACF authority + this PV's
        // live `.pvlist` ASG/ASL, so the CA server gates downstream reads
        // through `can_read` instead of granting every shadow PV a
        // permissive read.
        let access_hook = build_access_hook(
            acl.clone(),
            self.write_env.access.clone(),
            upstream_write.clone(),
        );
        // If a prior subscribe attempt left a stale shadow entry (it
        // would have been cleaned up by the failure path, but a hot
        // restart or a crashed task could orphan it), drop it before
        // re-registering. A genuine collision with a non-gateway
        // record/alias surfaces as `Err` and we propagate.
        //
        // registered under `served_name` (the alias) so a
        // downstream lookup of the alias resolves; the hook above still
        // forwards puts to `upstream_name` (the real PV).
        self.shadow_db.remove_simple_pv(served_name).await;
        if let Err(e) = self
            .shadow_db
            .add_pv_with_hooks(served_name, initial_value, hook, Some(access_hook))
            .await
        {
            return Err(BridgeError::PutRejected(format!(
                "shadow PV register failed: {e}"
            )));
        }

        // Subscribe (monitor receiver is independent of the channel handle).
        // On failure we MUST also remove the just-added shadow PV — otherwise
        // it lingers in `simple_pvs` with a hook pointing at a dead channel,
        // and the next downstream search resolves it without re-running
        // the resolver, leaving the gateway in a stuck state.
        let mut monitor = match channel.subscribe().await {
            Ok(m) => m,
            Err(e) => {
                self.shadow_db.remove_simple_pv(served_name).await;
                return Err(BridgeError::PutRejected(format!("subscribe failed: {e}")));
            }
        };

        // keep upstream_write up-to-date when the IOC's write-
        // access changes (e.g. ASG protection lockout). The C gateway's
        // accessCB (gatePv.cc:1851-1852) calls setWriteAccess + postAccessRights;
        // runtime re-notification to connected downstream clients requires a
        // CaServer::notify_access_change() API (deferred cross-crate).
        let access_rights_watcher = channel.on_access_rights_change({
            let flag = upstream_write.clone();
            move |rights| flag.store(rights.write, Ordering::Relaxed)
        });

        // Spawn forwarding task — does NOT borrow the channel, so the
        // direct put()/get() path can use the same channel without
        // contention. Auto-restart is handled by the loop below: if
        // the upstream monitor ends (closed channel, transient I/O
        // error), we re-subscribe with exponential backoff so the
        // shadow PV resumes receiving updates without the search
        // resolver having to re-issue the entire create_channel.
        let cache_clone = self.cache.clone();
        let db_clone = self.shadow_db.clone();
        let channel_for_task = channel.clone();
        let stats_for_task = self.write_env.stats.clone();
        let beacon_anomaly_for_task = self.write_env.beacon_anomaly.clone();
        // the forwarding task addresses the cache entry,
        // shadow PV, and alarm post by `served_name` — the same key the
        // shadow PV and cache were registered under above.
        let name = served_name.to_string();
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            loop {
                while let Some(result) = monitor.recv().await {
                    let snapshot = match result {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    stats_for_task.record_event();

                    // Update gateway cache.
                    //
                    // First event after Connecting / Disconnect:
                    //   * If subscribers are already attached
                    //     (downstream re-attached during the gap), go
                    //     straight to `Active` — naive demote to
                    //     `Inactive` would otherwise regress an
                    //     already-active PV every time the upstream
                    //     reconnects.
                    //   * Otherwise → `Inactive`.
                    let mut transitioned_from_disconnect = false;
                    if let Some(entry_arc) = cache_clone.read().await.get(&name) {
                        let mut entry = entry_arc.write().await;
                        let was_disconnect = matches!(entry.state, PvState::Disconnect);
                        if matches!(entry.state, PvState::Connecting | PvState::Disconnect) {
                            let next = if entry.subscriber_count() > 0 {
                                PvState::Active
                            } else {
                                PvState::Inactive
                            };
                            entry.set_state(next);
                        }
                        entry.update(snapshot.clone());
                        transitioned_from_disconnect = was_disconnect;
                    }

                    // trigger a beacon anomaly when the upstream
                    // reconnects so other gateway-aware clients
                    // re-discover and the downstream side knows the
                    // gateway is alive again. Mirrors C++ ca-gateway
                    // gateServer::generateBeaconAnomaly on reconnect.
                    if transitioned_from_disconnect {
                        beacon_anomaly_for_task.request();
                    }

                    // Push upstream snapshot (value + alarm + timestamp) to
                    // shadow PvDatabase so downstream monitors see the real
                    // upstream alarm state and IOC timestamp.
                    let post_result = db_clone
                        .put_pv_and_post_snapshot(&name, snapshot.clone())
                        .await;
                    // B5 RATE_STATS: count the monitor post fanned
                    // out downstream (C++ gateServer::postEventCount).
                    // Count only on a SUCCESSFUL fan-out so
                    // `postEventCount` stays consistent with
                    // `clientEventCount`, which counts successes — a
                    // failed post (e.g. shadow PV missing) forwarded
                    // nothing downstream.
                    match post_result {
                        Ok(()) => stats_for_task.record_post_event(),
                        Err(e) => tracing::debug!(
                            pv = %name,
                            error = %e,
                            "ca-gateway-rs: shadow put_pv_and_post_snapshot failed; \
                             postEventCount not incremented"
                        ),
                    }

                    // Re-arm the backoff after a successful event.
                    backoff = Duration::from_millis(250);
                }

                // Monitor closed — upstream disconnected. Mark cache
                // entry so any cached snapshot reads carry the right
                // state, and surface an INVALID alarm on the shadow
                // PV so downstream clients see the disconnect in
                // their alarm severity rather than continuing to
                // observe the last value at NoAlarm. C++
                // ca-gateway deletes the VC on Active→Disconnect
                // which yields ECA_DISCONN; the alarm-post route is
                // less disruptive and equivalent in operator visibility.
                if let Some(entry_arc) = cache_clone.read().await.get(&name) {
                    entry_arc.write().await.set_state(PvState::Disconnect);
                }
                // 3 = INVALID severity. LINK_ALARM (14) is the
                // correct EPICS status for a link-disconnect alarm;
                // upstream disconnect is visible to downstream monitors
                // both via severity and via the correct status code.
                let _ = db_clone
                    .post_alarm(
                        &name,
                        3,
                        epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                    )
                    .await;

                // Try to re-subscribe with exponential backoff. The
                // CaChannel itself drives reconnect under the hood;
                // this loop merely re-arms the monitor stream once
                // the channel is back up. Bail out only if the cache
                // entry has been evicted (i.e. nobody cares anymore).
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, max_backoff);

                if cache_clone.read().await.get(&name).is_none() {
                    return;
                }

                match channel_for_task.subscribe().await {
                    Ok(new_monitor) => {
                        monitor = new_monitor;
                        // The next successful event will flip state
                        // back to Inactive (see top of inner loop).
                    }
                    // `CaError::Shutdown` means the CA client
                    // coordinator is gone — `subscribe()` will never
                    // succeed again, so re-trying just spins this task
                    // at the backoff cap forever. Exit cleanly; the
                    // cache entry stays in Disconnect and the next
                    // search resolver pass re-creates the channel
                    // against a live client.
                    Err(CaError::Shutdown) => {
                        tracing::debug!(
                            pv = %name,
                            "ca-gateway-rs: CA coordinator gone, \
                             stopping upstream monitor task"
                        );
                        return;
                    }
                    Err(_) => {
                        // Transient failure — stay in Disconnect;
                        // another iteration of the outer loop will
                        // retry after the next sleep.
                        continue;
                    }
                }
            }
        });

        // subscription dedup keys on `served_name` so a
        // repeat search for the same alias is a no-op (fast path above)
        // and a `.pvlist` reload prunes by the served name it admits.
        self.subs.lock().insert(
            served_name.to_string(),
            UpstreamSubscription {
                channel,
                task,
                acl,
                _access_rights_watcher: access_rights_watcher,
            },
        );
        Ok(())
    }

    /// Remove an upstream subscription and abort its task. Also
    /// drops the corresponding shadow PV from the database so its
    /// (now-stale) `WriteHook` — which captures the soon-to-be-aborted
    /// upstream channel — cannot be invoked by a downstream client
    /// that opened the channel before the eviction landed.
    /// Mirrors C ca-gateway's `gatePvData::deactivate` cleanup.
    ///
    /// keyed by the *served* name — both `subs` and the
    /// shadow PV are registered under the downstream-facing name (alias
    /// or real), so callers (`.pvlist` reload prune, `sweep_orphaned`)
    /// pass the served name they read from the cache / subs map.
    pub async fn unsubscribe(&self, served_name: &str) {
        let removed = self.subs.lock().remove(served_name);
        if let Some(sub) = removed {
            sub.task.abort();
        }
        // Best-effort: if the PV is gone already (concurrent reload
        // path) `remove_simple_pv` returns None; we don't care.
        let _ = self.shadow_db.remove_simple_pv(served_name).await;
    }

    /// Forward a put operation to the upstream IOC. Reuses the
    /// existing subscribed channel when available, avoiding a
    /// fresh CREATE_CHAN round-trip per write. Falls back to a
    /// transient channel only when the PV has no subscription.
    pub async fn put(&self, upstream_name: &str, value: &EpicsValue) -> BridgeResult<()> {
        let channel_for_op = self
            .subs
            .lock()
            .get(upstream_name)
            .map(|s| s.channel.clone());
        if let Some(ch) = channel_for_op {
            return ch
                .put(value)
                .await
                .map_err(|e| BridgeError::PutRejected(format!("upstream put: {e}")));
        }
        let channel = self.client.create_channel(upstream_name);
        channel
            .put(value)
            .await
            .map_err(|e| BridgeError::PutRejected(format!("upstream put: {e}")))
    }

    /// Get the current value from upstream. Reuses the subscribed
    /// channel when available; otherwise opens a transient one.
    pub async fn get(&self, upstream_name: &str) -> BridgeResult<EpicsValue> {
        let channel_for_op = self
            .subs
            .lock()
            .get(upstream_name)
            .map(|s| s.channel.clone());
        if let Some(ch) = channel_for_op {
            let (_dbf, value) = ch
                .get()
                .await
                .map_err(|e| BridgeError::PutRejected(format!("upstream get: {e}")))?;
            return Ok(value);
        }
        let channel = self.client.create_channel(upstream_name);
        let (_dbf, value) = channel
            .get()
            .await
            .map_err(|e| BridgeError::PutRejected(format!("upstream get: {e}")))?;
        Ok(value)
    }

    /// Live `.pvlist` ASG/ASL currently enforced for the shadow PV keyed
    /// by `served_name` (the subscription key). Reflects the latest
    /// [`update_acl`](Self::update_acl) from an `AS`/`PVL` reload, not
    /// just the value resolved at first subscription. Used by tests +
    /// diagnostics.
    pub fn asg_for(&self, served_name: &str) -> Option<(Option<String>, i32)> {
        self.subs.lock().get(served_name).map(|s| {
            let acl = s.acl.load();
            (acl.asg.clone(), acl.asl)
        })
    }

    /// Replace the live ASG/ASL for the still-admitted shadow PV keyed by
    /// `served_name`. Returns `true` if the subscription exists and its
    /// identity actually changed (caller can use this to decide whether a
    /// downstream access-rights re-notification is warranted).
    ///
    /// This is the owner of the per-PV ACL transition during an
    /// `AS`/`PVL` reload: it stores a new `PvAcl` into the shared cell so
    /// the read and write hooks — which `load()` it on every access —
    /// immediately enforce the new group/level. Mirrors C ca-gateway
    /// `gateServer::newAs` reinstalling the new `gateAsEntry` on each
    /// still-allowed PV (gateServer.cc:603-630).
    ///
    /// runtime re-notification of *already-connected* downstream clients
    /// (C `gateChan::resetAsClient` posting an access-rights event,
    /// gateVc.cc:170-199) requires a `CaServer::notify_access_change()`
    /// API and is deferred cross-crate — see the `on_access_rights_change`
    /// note in `ensure_subscribed`. New connections re-evaluate the access
    /// hook and therefore already see the updated rights.
    pub fn update_acl(&self, served_name: &str, asg: Option<String>, asl: i32) -> bool {
        let subs = self.subs.lock();
        match subs.get(served_name) {
            Some(sub) => {
                let next = PvAcl { asg, asl };
                let prev = sub.acl.load();
                if **prev == next {
                    return false;
                }
                sub.acl.store(Arc::new(next));
                true
            }
            None => false,
        }
    }

    /// Sweep cache and remove upstream subscriptions for entries that
    /// no longer exist in the cache (e.g., evicted by cleanup).
    pub async fn sweep_orphaned(&self) {
        let cache = self.cache.read().await;
        let orphans: Vec<String> = self
            .subs
            .lock()
            .keys()
            .filter(|name| cache.get(name).is_none())
            .cloned()
            .collect();
        drop(cache);

        for name in orphans {
            self.unsubscribe(&name).await;
        }
    }

    /// Abort all active subscriptions and shut down.
    pub async fn shutdown(&self) {
        let drained: Vec<UpstreamSubscription> =
            self.subs.lock().drain().map(|(_, sub)| sub).collect();
        for sub in drained {
            sub.task.abort();
        }
        self.client.shutdown().await;
    }
}

/// build the per-PV access hook the CA server's
/// `compute_access` consults to report read/write rights and gate
/// downstream reads. Symmetric to [`build_write_hook`]: it captures the
/// same single `ArcSwap<AccessConfig>` and the PV's `.pvlist` ASG/ASL,
/// so a downstream `caget`/`camonitor` is gated by the same `can_read`
/// the operator configured for `caput` — closing the gap where an ACF
/// read deny still let reads through because the read path never
/// consulted ACF.
///
/// `read` comes straight from `can_read`. `write` mirrors the write
/// hook's empty-`user` guard: a client that never sent `CLIENT_NAME`
/// (no identity) is reported write-denied whenever rules are loaded, so
/// the access-rights report matches what the write hook will actually
/// enforce.
///
/// `upstream_write` mirrors the upstream IOC's `ca_write_access(chID)`,
/// set at connect time and kept live by `on_access_rights_change`. The write
/// decision is now `local_acf_write && upstream_write`, matching C
/// `gateVcChan::writeAccess` (gateVc.cc:341): `asclient->writeAccess() && vc->writeAccess()`.
fn build_access_hook(
    acl: PvAclCell,
    access: Arc<ArcSwap<AccessConfig>>,
    upstream_write: Arc<AtomicBool>,
) -> epics_base_rs::server::pv::AccessHook {
    Arc::new(move |user: &str, host: &str| {
        let cfg = access.load();
        // Load the live `.pvlist` identity each call so a reloaded
        // ASG/ASL is enforced immediately.
        let pv_acl = acl.load();
        let asg_ref = pv_acl.asg.as_deref().unwrap_or("DEFAULT");
        let asl = pv_acl.asl;
        let read = cfg.can_read(asg_ref, asl, user, host);
        let local_write = if user.is_empty() && cfg.has_rules() {
            false
        } else {
            cfg.can_write(asg_ref, asl, user, host)
        };
        let write = local_write && upstream_write.load(Ordering::Relaxed);
        epics_base_rs::server::pv::AccessDecision { read, write }
    })
}

/// Build the [`WriteHook`] closure for one upstream PV. Called once
/// per `ensure_subscribed`; the resulting `Arc<dyn Fn …>` is installed
/// on the shadow `ProcessVariable` so every client `caput` runs this
/// pipeline.
///
/// `served_name` is the cache/shadow-DB key (== `upstream_name` when no
/// alias) used to read the prior cached value for the putlog `old=`
/// field; `pv_name` is the upstream PV name written to the putlog.
///
/// Pipeline (matches C ca-gateway `gatePvData::putCB` ordering):
/// 1. Read-only mode → reject + record stat + putlog
/// 2. Host-based DENY (pvlist `FROM host`) → reject + putlog
/// 3. ACF `can_write(asg, asl, user, host)` → reject + putlog
/// 4. Forward `caput` to upstream via the shared channel
/// 5. Putlog the outcome (Ok/Failed) and bump put-count stat
fn build_write_hook(
    served_name: String,
    pv_name: String,
    channel: Arc<CaChannel>,
    acl: PvAclCell,
    env: WriteHookEnv,
) -> WriteHook {
    Arc::new(move |new_value: EpicsValue, ctx: WriteContext| {
        let served_name = served_name.clone();
        let pv_name = pv_name.clone();
        let channel = channel.clone();
        let acl = acl.clone();
        let env = env.clone();
        Box::pin(async move {
            // Bound the audit-log value so a client putting a 1M
            // element waveform doesn't allocate a 25MB String per
            // put and write a multi-megabyte putlog line. 256 chars
            // is enough for scalars, NTScalar, and a leading slice
            // of array values; full fidelity would belong in a
            // separate binary trace if ever needed.
            let value_str = format_value_for_audit(&new_value, 256);

            // Prior cached value for the audit `old=` field. C ca-gateway
            // logs `vc->eventData()` — the virtual connection's cached
            // monitor value — as the put's `old` value
            // (gateResources.cc:486-492). Read once up-front, keyed by
            // `served_name` (the cache key, upstream.rs:313/481), so every
            // log path (denial branches + forward outcome) records the
            // same pre-put value, and only when a putlog is configured.
            let old_str = if env.putlog.is_some() {
                cached_old_for_audit(&env, &served_name).await
            } else {
                String::new()
            };

            // 1. read-only mode — gateway-wide flag.
            if env.read_only {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::ReadOnlyField(format!(
                    "{pv_name} (gateway in read-only mode)"
                )));
            }

            // 2. pvlist host-based DENY — surface as PutDisabled so the
            // ECA status differs from "ACL deny" and operators can
            // distinguish in audits. `load_full` is wait-free.
            let pvlist = env.pvlist.load_full();
            if pvlist.is_host_denied(&pv_name, &ctx.host) {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::PutDisabled(format!(
                    "{pv_name} (host {} denied by pvlist)",
                    ctx.host
                )));
            }

            // 3. AccessConfig — the actual ACF access-rights check.
            // Empty `user` (CA client never sent CLIENT_NAME) is a
            // protocol-violation signal: refuse the put unless the ACF
            // is in the explicit allow-all configuration. This blocks
            // a malformed/adversarial client that fires WRITE_NOTIFY
            // before HOST_NAME/CLIENT_NAME from being matched as
            // "anonymous" against UAG groups.
            let access = env.access.load_full();
            if ctx.user.is_empty() && access.has_rules() {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::ReadOnlyField(format!(
                    "{pv_name} (no client identity)"
                )));
            }
            // Load the live `.pvlist` identity per put so a reloaded
            // ASG/ASL gates the write immediately.
            let pv_acl = acl.load();
            let asg_ref = pv_acl.asg.as_deref().unwrap_or("DEFAULT");
            let asl = pv_acl.asl;
            if !access.can_write(asg_ref, asl, &ctx.user, &ctx.host) {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::ReadOnlyField(format!(
                    "{pv_name} (asg {asg_ref}, user {})",
                    ctx.user
                )));
            }

            // 4. Forward upstream — propagate CaError directly so the
            // CA TCP write path surfaces the right ECA status to the
            // caller (e.g. ECA_TIMEOUT, ECA_DISCONN).
            let result = channel.put(&new_value).await;

            // 5. Putlog + stats. PutLog write errors are surfaced via
            // tracing (not just `let _ =`) so a disk-full audit
            // trail is visible to operators.
            if let Some(pl) = &env.putlog {
                let outcome = if result.is_ok() {
                    PutOutcome::Ok
                } else {
                    PutOutcome::Failed
                };
                if let Err(e) = pl
                    .log(
                        &ctx.user, &ctx.host, &pv_name, &value_str, &old_str, outcome,
                    )
                    .await
                {
                    tracing::warn!(
                        target: "ca_gateway::putlog",
                        error = %e,
                        "ca-gateway-rs: putlog write failed"
                    );
                }
            }
            if result.is_ok() {
                env.stats.record_put();
            }
            result
        })
    })
}

/// Read the gateway's last-cached upstream value for `key` and render it
/// for the put-audit `old=` field. Mirrors C ca-gateway, which logs
/// `vc->eventData()` (the virtual connection's cached monitor value) as
/// the put's `old` value (gateResources.cc:486-492). `key` is the
/// `served_name` the cache and forwarding task register under
/// (upstream.rs:313/481). Returns `"?"` when no monitor update has
/// populated the cache yet, matching C's `old_value == NULL` →
/// `acOldVal = "?"` (gateResources.cc:476-480).
async fn cached_old_for_audit(env: &WriteHookEnv, key: &str) -> String {
    let entry = env.cache.read().await.get(key);
    if let Some(entry) = entry
        && let Some(snap) = &entry.read().await.cached
    {
        return format_value_for_audit(&snap.value, 256);
    }
    "?".to_string()
}

/// Helper: emit a single `Denied` putlog line. Called from each
/// rejection branch in the WriteHook so the structure is uniform
/// (timestamp, user@host, pv, value, old, DENIED). `old` is the prior
/// cached value the caller read up-front (see [`cached_old_for_audit`]).
/// Errors from the log write itself are surfaced via `tracing` (debounced
/// via target) so a disk-full putlog doesn't silently disappear the audit
/// trail.
async fn log_denial(env: &WriteHookEnv, ctx: &WriteContext, pv: &str, value: &str, old: &str) {
    if let Some(pl) = &env.putlog
        && let Err(e) = pl
            .log(&ctx.user, &ctx.host, pv, value, old, PutOutcome::Denied)
            .await
    {
        tracing::warn!(
            target: "ca_gateway::putlog",
            error = %e,
            "ca-gateway-rs: putlog write failed"
        );
    }
}

/// Render an `EpicsValue` for the put-audit log, truncating to at
/// most `max_len` characters with an ellipsis suffix when needed.
/// Putlog lines are shipped to disk synchronously per put, so a
/// 1M-element waveform with the default `Display` would balloon to
/// tens of MB per write — both a perf disaster and a disk-fill
/// vector. The truncated form is enough to distinguish scalar puts
/// in operator-facing audits; full-fidelity tracing belongs
/// elsewhere.
fn format_value_for_audit(v: &EpicsValue, max_len: usize) -> String {
    // bound the formatted-string allocation BEFORE running
    // Display::fmt over the whole value. The previous
    // `format!("{v}")` ran the full Display implementation first
    // (every element of a million-element waveform) then truncated
    // — a 25 MB String per put on a 1 M-element double array, with
    // no caller bound. For arrays, slice to a small head before
    // formatting so the heaviest path stays O(max_len) rather than
    // O(array_len). For scalars / strings the overhead is at most
    // one short String.
    const HEAD_PEEK_ELEMS: usize = 32;
    let truncated;
    let v_for_format: &EpicsValue = match v {
        EpicsValue::ShortArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::ShortArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        EpicsValue::FloatArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::FloatArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        EpicsValue::EnumArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::EnumArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        EpicsValue::DoubleArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::DoubleArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        EpicsValue::LongArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::LongArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        EpicsValue::CharArray(arr) if arr.len() > max_len => {
            truncated = EpicsValue::CharArray(arr[..max_len].to_vec());
            &truncated
        }
        EpicsValue::StringArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::StringArray(arr[..HEAD_PEEK_ELEMS].to_vec());
            &truncated
        }
        _ => v,
    };
    let s = format!("{v_for_format}");
    if s.len() <= max_len {
        s
    } else {
        // Truncate at a char boundary so we don't split a UTF-8
        // codepoint mid-byte (rare for numeric arrays but cheap
        // safety).
        let mut end = max_len.saturating_sub(3);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_ca_rs::server::CaServer;
    use serial_test::serial;

    fn dummy_env() -> WriteHookEnv {
        WriteHookEnv {
            read_only: false,
            access: Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all())),
            pvlist: Arc::new(ArcSwap::from_pointee(PvList::new())),
            putlog: None,
            cache: Arc::new(RwLock::new(PvCache::new())),
            stats: Arc::new(Stats::new("gw:".into())),
            beacon_anomaly: Arc::new(crate::ca_gateway::beacon::BeaconAnomaly::new()),
        }
    }

    /// Boundary coverage for the put-audit `old=` source
    /// ([`cached_old_for_audit`]): cache miss, entry-without-snapshot, and
    /// entry-with-snapshot. Mirrors C ca-gateway's `?` for a NULL
    /// `vc->eventData()` versus the rendered cached value.
    #[tokio::test]
    async fn cached_old_for_audit_boundaries() {
        use epics_base_rs::server::snapshot::Snapshot;
        let env = dummy_env();

        // (a) no cache entry under the key → "?" (C: old_value == NULL).
        assert_eq!(cached_old_for_audit(&env, "GW:MISS").await, "?");

        // (b) entry exists but no monitor value cached yet → "?".
        env.cache.write().await.get_or_create("GW:PV");
        assert_eq!(cached_old_for_audit(&env, "GW:PV").await, "?");

        // (c) entry with a cached snapshot → rendered value.
        {
            let cache = env.cache.read().await;
            let entry = cache.get("GW:PV").expect("entry created above");
            entry.write().await.update(Snapshot::new(
                EpicsValue::Double(24.8),
                0,
                0,
                std::time::SystemTime::now(),
            ));
        }
        assert_eq!(cached_old_for_audit(&env, "GW:PV").await, "24.8");
    }

    /// Reserve a free TCP port by binding ephemeral then dropping.
    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("reserve free CA port")
            .local_addr()
            .unwrap()
            .port()
    }

    /// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port` so the
    /// `UpstreamManager`'s internal env-driven `CaClient::new()` connects
    /// to the test server. Callers must be `#[serial(epics_env)]`.
    fn pin_env(port: u16) {
        // SAFETY: env-touching tests are serialized via
        // `#[serial(epics_env)]`; no other thread reads/writes these
        // vars concurrently.
        unsafe {
            std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
        }
    }

    /// Build an `UpstreamManager` whose internal `CaClient::new()` reads
    /// the ambient `EPICS_CA_*` env — call [`pin_env`] first so the client
    /// is pinned to the test server.
    async fn pinned_manager(db: Arc<PvDatabase>) -> UpstreamManager {
        let env = dummy_env();
        UpstreamManager::new(UpstreamManagerConfig {
            cache: Arc::new(RwLock::new(PvCache::new())),
            shadow_db: db,
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            stats: env.stats.clone(),
            read_only: false,
            connect_timeout: Duration::from_secs(1),
            beacon_anomaly: env.beacon_anomaly.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        })
        .await
        .expect("manager builds")
    }

    #[tokio::test]
    async fn manager_construct() {
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let db = Arc::new(PvDatabase::new());
        let env = dummy_env();
        let mgr = UpstreamManager::new(UpstreamManagerConfig {
            cache,
            shadow_db: db,
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            stats: env.stats.clone(),
            read_only: false,
            connect_timeout: Duration::from_secs(1),
            beacon_anomaly: env.beacon_anomaly.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        })
        .await;
        assert!(mgr.is_ok());

        let mgr = mgr.unwrap();
        assert_eq!(mgr.subscription_count(), 0);
        assert!(!mgr.is_subscribed("ANY"));
    }

    /// B10: an upstream `CaClient` constructed with a TLS client
    /// config must build successfully. The config is plumbed through
    /// `UpstreamManagerConfig` and reaches `CaClient::new_with_config`.
    /// We do not connect to a real IOC here — the assertion is that
    /// the TLS-configured construction path compiles and runs.
    #[cfg(feature = "ca-gateway-tls")]
    #[tokio::test]
    async fn manager_construct_with_upstream_tls() {
        use epics_ca_rs::tls::{Roots, TlsConfig};

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let db = Arc::new(PvDatabase::new());
        let env = dummy_env();
        // An empty root store is sufficient to exercise the
        // server-auth client-config build path — no actual TLS
        // handshake happens without a connection.
        let tls = TlsConfig::client_from_roots(Roots::empty());
        let mgr = UpstreamManager::new(UpstreamManagerConfig {
            cache,
            shadow_db: db,
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            stats: env.stats.clone(),
            read_only: false,
            connect_timeout: Duration::from_secs(1),
            beacon_anomaly: env.beacon_anomaly.clone(),
            upstream_tls: Some(tls),
            upstream_tls_server_name: Some("ioc.example.com".to_string()),
        })
        .await;
        assert!(
            mgr.is_ok(),
            "upstream TLS-configured CaClient failed to build: {:?}",
            mgr.err()
        );
        assert_eq!(mgr.unwrap().subscription_count(), 0);
    }

    #[test]
    fn _entry_imports() {
        // Sanity check that the cache types are in scope
        let _ = super::super::cache::GwPvEntry::new_connecting("X");
    }

    /// the access hook reports read/write rights through
    /// the gateway's ACF + `.pvlist` ASG, so the CA server can gate a
    /// downstream `caget`/`camonitor` the same way the write hook gates
    /// `caput`. ACF grants READ to `alice` only and WRITE to nobody.
    #[test]
    fn br_fr1_access_hook_routes_read_through_acf() {
        let acf = r#"
UAG(ops) { alice }
ASG(DEFAULT) {
    RULE(0, READ) { UAG(ops) }
}
"#;
        let access = Arc::new(ArcSwap::from_pointee(
            AccessConfig::from_string(acf).expect("ACF parses"),
        ));
        let upstream_write = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl {
            asg: Some("DEFAULT".to_string()),
            asl: 0,
        }));
        let hook = build_access_hook(acl, access, upstream_write);

        // Privileged user: read granted, write denied (no WRITE rule).
        let alice = hook("alice", "host1");
        assert!(alice.read, "alice is in UAG(ops) → READ");
        assert!(!alice.write, "no WRITE rule → write denied even for alice");

        // Unprivileged user: read AND write denied — this is the gap
        // FR-1 closes (previously every shadow PV read was permissive).
        let intruder = hook("intruder", "host1");
        assert!(!intruder.read, "intruder is not in UAG(ops) → no READ");
        assert!(!intruder.write, "intruder → no WRITE");

        // No client identity: read evaluates normally (denied here, no
        // matching UAG), write is force-denied while rules are loaded —
        // mirrors the write hook's empty-user guard.
        let anon = hook("", "host1");
        assert!(!anon.read, "empty user matches no UAG → no READ");
        assert!(!anon.write, "empty user + rules → write force-denied");
    }

    /// with no ACF (allow-all), the hook grants both —
    /// the gateway's default permissive posture is unchanged.
    #[test]
    fn br_fr1_access_hook_allow_all_grants_both() {
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let upstream_write = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl { asg: None, asl: 0 }));
        let hook = build_access_hook(acl, access, upstream_write);
        let d = hook("anyone", "anywhere");
        assert!(d.read && d.write, "allow-all must grant read and write");
    }

    /// when upstream write-access is denied (e.g. upstream IOC
    /// ACF), the access hook must report write=false regardless of local
    /// ACF — mirrors C gateVcChan::writeAccess (gateVc.cc:341):
    /// asclient->writeAccess() && vc->writeAccess().
    #[test]
    fn br_r51_upstream_write_denied_overrides_local_acf_allow() {
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let upstream_write = Arc::new(AtomicBool::new(false));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl { asg: None, asl: 0 }));
        let hook = build_access_hook(acl, access, upstream_write.clone());

        let d = hook("alice", "host1");
        assert!(d.read, "read must still be granted by allow-all");
        assert!(
            !d.write,
            "upstream write-denied must override local allow-all"
        );

        // Restoring upstream write-access restores write permission.
        upstream_write.store(true, Ordering::Relaxed);
        let d2 = hook("alice", "host1");
        assert!(d2.write, "write must be granted once upstream restores it");
    }

    /// the access hook must enforce the LIVE per-PV ASG/ASL, not the one
    /// captured when the hook was built. An `AS`/`PVL` reload that moves a
    /// still-admitted PV from `OldGroup` to `NewGroup` swaps the shared
    /// ACL cell; the same hook (never rebuilt) must then compute against
    /// the new group. Pre-fix the hook captured ASG/ASL by value and kept
    /// enforcing the stale identity.
    #[test]
    fn br_2026_111_access_hook_follows_live_acl_swap() {
        let acf = r#"
UAG(ops)    { alice }
UAG(others) { bob }
ASG(OldGroup) {
    RULE(1, READ) { UAG(ops) }
}
ASG(NewGroup) {
    RULE(1, READ) { UAG(others) }
}
"#;
        let access = Arc::new(ArcSwap::from_pointee(
            AccessConfig::from_string(acf).expect("ACF parses"),
        ));
        let upstream_write = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl {
            asg: Some("OldGroup".to_string()),
            asl: 1,
        }));
        let hook = build_access_hook(acl.clone(), access, upstream_write);

        // Under OldGroup, alice has READ; bob does not.
        assert!(hook("alice", "h").read, "OldGroup grants alice READ");
        assert!(!hook("bob", "h").read, "OldGroup denies bob READ");

        // Simulate the reload: swap the live ACL cell the hook holds.
        acl.store(Arc::new(PvAcl {
            asg: Some("NewGroup".to_string()),
            asl: 1,
        }));

        // The same hook now computes against NewGroup: rights flip.
        assert!(
            !hook("alice", "h").read,
            "after live ACL swap to NewGroup, alice's READ is revoked"
        );
        assert!(
            hook("bob", "h").read,
            "after live ACL swap to NewGroup, bob gains READ"
        );
    }

    /// the manager's reload owner path: `update_acl` swaps the live ASG/ASL
    /// of a still-admitted shadow PV and `asg_for` reflects it immediately.
    /// Re-applying the same identity reports no change (idempotent), so a
    /// caller can gate a downstream access-rights re-notification on a real
    /// transition.
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_2026_111_reload_updates_live_acl_on_admitted_pv() {
        let name = "AS:reload:pv";
        let port = free_port();
        let server = CaServer::builder()
            .port(port)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let _server = tokio::spawn(async move { server.run().await });
        tokio::time::sleep(Duration::from_millis(300)).await;

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;

        mgr.ensure_subscribed(name, name, Some("OldGroup".to_string()), 1)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");
        assert_eq!(
            mgr.asg_for(name),
            Some((Some("OldGroup".to_string()), 1)),
            "initial ASG/ASL is the value resolved at subscribe"
        );

        // Reload moves the still-admitted PV to a new ASG/ASL.
        assert!(
            mgr.update_acl(name, Some("NewGroup".to_string()), 0),
            "a real ASG/ASL change reports true"
        );
        assert_eq!(
            mgr.asg_for(name),
            Some((Some("NewGroup".to_string()), 0)),
            "asg_for follows the reload immediately"
        );

        // Idempotent: re-applying the identical identity is a no-op.
        assert!(
            !mgr.update_acl(name, Some("NewGroup".to_string()), 0),
            "an unchanged ASG/ASL reports false"
        );

        mgr.shutdown().await;
    }

    /// when a downstream client searches for a `.pvlist`
    /// ALIAS, the gateway must register the shadow PV under the *served*
    /// (alias) name so the client's `CREATE_CHANNEL`/read resolves, while
    /// the upstream subscription targets the *real* PV. Pre-fix the shadow
    /// PV was keyed by the real name, so an alias lookup missed.
    ///
    /// Registration is now gated on a live upstream connection, so this
    /// test hosts a real `CaServer` serving the *real* name; the served
    /// (alias) name is what the shadow PV must be keyed by.
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_fr2_alias_registers_shadow_pv_under_served_name() {
        // Served (alias) name differs from the resolved real PV.
        let served = "Beam:current";
        let real = "SR:DCCT:current";

        let port = free_port();
        let server = CaServer::builder()
            .port(port)
            .pv(real, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let _server = tokio::spawn(async move { server.run().await });
        tokio::time::sleep(Duration::from_millis(300)).await;

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;

        mgr.ensure_subscribed(served, real, None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        // The downstream-facing entry is keyed by the alias the client
        // searched for — this is the FR-2 fix.
        assert!(
            db.find_pv(served).await.is_some(),
            "shadow PV must be registered under the served (alias) name"
        );
        // The real upstream name is NOT exposed downstream (the client
        // never learns it; only the alias is served).
        assert!(
            db.find_pv(real).await.is_none(),
            "real upstream name must not be a downstream entry"
        );
        // Subscription dedup also keys on the served name.
        assert!(
            mgr.is_subscribed(served),
            "subscription must be tracked under the served name"
        );
        assert!(
            !mgr.is_subscribed(real),
            "subscription must not be keyed by the real upstream name"
        );

        mgr.shutdown().await;
    }

    /// a plain (non-alias) ALLOW match passes served == real,
    /// so the shadow PV is keyed by the same name — the pre-FR-2 behavior
    /// for non-alias names is preserved exactly.
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_fr2_non_alias_keys_shadow_pv_by_same_name() {
        let name = "Plain:pv";

        let port = free_port();
        let server = CaServer::builder()
            .port(port)
            .pv(name, EpicsValue::Double(2.0))
            .build()
            .await
            .expect("CA server");
        let _server = tokio::spawn(async move { server.run().await });
        tokio::time::sleep(Duration::from_millis(300)).await;

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        assert!(db.find_pv(name).await.is_some(), "served == real registers");
        assert!(mgr.is_subscribed(name), "subscription tracked under name");

        mgr.shutdown().await;
    }

    /// when the upstream never connects, `ensure_subscribed` must
    /// treat the search as a miss — return `Err`, register no shadow PV,
    /// and track no subscription — so the gateway answers does-not-exist
    /// instead of black-holing a name that merely matches a `.pvlist`
    /// pattern. Mirrors C ca-gateway `gatePvData::death →
    /// pverDoesNotExistHere` (gatePv.cc:622): exists-here is reported only
    /// after the upstream connects, not on a bare pattern match.
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_r64_dead_upstream_not_registered() {
        // A free port with NO server bound: the upstream search never
        // resolves and the configured connect_timeout expires.
        let port = free_port();
        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;

        let name = "Ghost:pv";
        let result = mgr.ensure_subscribed(name, name, None, 0).await;

        assert!(
            result.is_err(),
            "ensure_subscribed must fail when the upstream never connects"
        );
        assert!(
            db.find_pv(name).await.is_none(),
            "no shadow PV may be registered for a never-connected upstream"
        );
        assert!(
            !mgr.is_subscribed(name),
            "no subscription may be tracked for a never-connected upstream"
        );

        mgr.shutdown().await;
    }

    /// the lazy-resolution connect gate must honor the configured
    /// `CacheTimeouts::connect_timeout`, not a hard-coded constant. Build
    /// a manager with a short 150 ms budget against a dead port and assert
    /// the search miss is reported well under the old 1 s constant —
    /// proving the configured value flows into `wait_connected` (one
    /// connect-timeout owner shared with the cache reaper).
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_2026_22_lazy_connect_honors_configured_timeout() {
        let port = free_port();
        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let env = dummy_env();
        let mgr = UpstreamManager::new(UpstreamManagerConfig {
            cache: Arc::new(RwLock::new(PvCache::new())),
            shadow_db: db.clone(),
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            stats: env.stats.clone(),
            read_only: false,
            connect_timeout: Duration::from_millis(150),
            beacon_anomaly: env.beacon_anomaly.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        })
        .await
        .expect("manager builds");

        let start = tokio::time::Instant::now();
        let result = mgr
            .ensure_subscribed("Ghost:cfg", "Ghost:cfg", None, 0)
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "dead upstream must miss");
        assert!(
            elapsed < Duration::from_millis(800),
            "configured 150ms connect_timeout must govern (not the old 1s \
             constant); elapsed = {elapsed:?}"
        );

        mgr.shutdown().await;
    }
}
