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
use std::time::Duration;

use arc_swap::ArcSwap;
use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{WriteContext, WriteHook};
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::error::{BridgeError, BridgeResult};

use super::access::AccessConfig;
use super::cache::{PvCache, PvState};
use super::putlog::{PutLog, PutOutcome};
use super::pvlist::PvList;
use super::stats::Stats;

/// One upstream subscription: the long-lived [`CaChannel`] is shared
/// between the monitor-forwarding task and any direct
/// [`UpstreamManager::put`] / [`UpstreamManager::get`] calls so we
/// don't pay a fresh CREATE_CHAN round-trip per write. Mirrors C
/// CA-gateway behaviour (cas/io/casChannel.cc) where the upstream
/// channel is reused across the gateway's lifetime.
struct UpstreamSubscription {
    channel: Arc<CaChannel>,
    task: JoinHandle<()>,
    /// Resolved access security group from the matching `.pvlist` rule.
    /// Cached here so the write hook can call `AccessConfig::can_write`
    /// without re-resolving on every put.
    asg: Option<String>,
    /// Resolved access security level (paired with `asg`).
    asl: i32,
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
    /// Stats counters.
    stats: Arc<Stats>,
    /// Beacon-anomaly trigger handle. Fires from
    /// `ensure_subscribed`'s first-Active transition (so other gateway-
    /// aware downstream clients re-search and discover this gateway as
    /// the server for the just-added PV) and from upstream-reconnect
    /// transitions in the forwarding task. Mirrors C++ ca-gateway's
    /// `gateServer::generateBeaconAnomaly` (B-G9).
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
    pub beacon_anomaly: Arc<super::beacon::BeaconAnomaly>,
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
        let client = CaClient::new()
            .await
            .map_err(|e| BridgeError::PutRejected(format!("CaClient init: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
            cache: cfg.cache,
            shadow_db: cfg.shadow_db,
            write_env: WriteHookEnv {
                read_only: cfg.read_only,
                access: cfg.access,
                pvlist: cfg.pvlist,
                putlog: cfg.putlog,
                stats: cfg.stats,
                beacon_anomaly: cfg.beacon_anomaly,
            },
            subs: parking_lot::Mutex::new(HashMap::new()),
            pending: parking_lot::Mutex::new(HashMap::new()),
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
    pub async fn ensure_subscribed(
        &self,
        upstream_name: &str,
        asg: Option<String>,
        asl: i32,
    ) -> BridgeResult<()> {
        // Fast path: already subscribed.
        if self.subs.lock().contains_key(upstream_name) {
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
            if let Some(existing) = pending.get(upstream_name) {
                Decision::WaitFor(existing.clone())
            } else {
                let n = Arc::new(tokio::sync::Notify::new());
                pending.insert(upstream_name.to_string(), n.clone());
                Decision::Owner(n)
            }
        };
        let dedup_notify = match decision {
            Decision::WaitFor(n) => {
                n.notified().await;
                let already = self.subs.lock().contains_key(upstream_name);
                return if already {
                    Ok(())
                } else {
                    Err(BridgeError::PutRejected(format!(
                        "upstream subscribe failed (peer creator): {upstream_name}"
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
            key: upstream_name,
            notify: dedup_notify.clone(),
        };

        // Add entry to cache (or get existing) and reset to Connecting
        {
            let mut cache = self.cache.write().await;
            let entry = cache.get_or_create(upstream_name);
            entry.write().await.set_state(PvState::Connecting);
        }

        // Create one CA channel and reuse it for both monitor and
        // direct PUT/GET. Stored as Arc so the lifecycle guard fires
        // exactly once when the subscription is dropped.
        let channel = Arc::new(self.client.create_channel(upstream_name));

        // DBR negotiation: best-effort initial GET so the shadow
        // PV's first registered type matches upstream's native type.
        // Falls back to a Double placeholder if the get fails or
        // times out — the first monitor event will overwrite the
        // value either way. The timeout/error is logged at INFO so
        // an operator chasing type-mismatch confusion can correlate
        // a confused downstream introspect with its upstream miss.
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

        // Atomically register the shadow PV WITH its WriteHook
        // attached. `add_pv_with_hook` constructs the PV with the
        // hook installed before inserting into `simple_pvs`, so a
        // downstream client cannot race a CREATE_CHAN + WRITE_NOTIFY
        // into the small window where the PV is findable but the
        // hook isn't yet bound (which would silently drop the put
        // into the local `pv.set()` fallback).
        let hook = build_write_hook(
            upstream_name.to_string(),
            channel.clone(),
            asg.clone(),
            asl,
            self.write_env.clone(),
        );
        // If a prior subscribe attempt left a stale shadow entry (it
        // would have been cleaned up by the failure path, but a hot
        // restart or a crashed task could orphan it), drop it before
        // re-registering. A genuine collision with a non-gateway
        // record/alias surfaces as `Err` and we propagate.
        self.shadow_db.remove_simple_pv(upstream_name).await;
        if let Err(e) = self
            .shadow_db
            .add_pv_with_hook(upstream_name, initial_value, hook)
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
                self.shadow_db.remove_simple_pv(upstream_name).await;
                return Err(BridgeError::PutRejected(format!("subscribe failed: {e}")));
            }
        };

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
        let name = upstream_name.to_string();
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

                    // B-G9: trigger a beacon anomaly when the upstream
                    // reconnects so other gateway-aware clients
                    // re-discover and the downstream side knows the
                    // gateway is alive again. Mirrors C++ ca-gateway
                    // gateServer::generateBeaconAnomaly on reconnect.
                    if transitioned_from_disconnect {
                        beacon_anomaly_for_task.request();
                    }

                    // Push to shadow PvDatabase to fan out to downstream clients
                    let _ = db_clone
                        .put_pv_and_post(&name, snapshot.value.clone())
                        .await;
                    // B5 RATE_STATS: count the monitor post fanned
                    // out downstream (C++ gateServer::postEventCount).
                    stats_for_task.record_post_event();

                    // Re-arm the backoff after a successful event.
                    backoff = Duration::from_millis(250);
                }

                // Monitor closed — upstream disconnected. Mark cache
                // entry so any cached snapshot reads carry the right
                // state, and surface an INVALID alarm on the shadow
                // PV so downstream clients see the disconnect in
                // their alarm severity rather than continuing to
                // observe the last value at NoAlarm (B-G11). C++
                // ca-gateway deletes the VC on Active→Disconnect
                // which yields ECA_DISCONN; the alarm-post route is
                // less disruptive and equivalent in operator visibility.
                if let Some(entry_arc) = cache_clone.read().await.get(&name) {
                    entry_arc.write().await.set_state(PvState::Disconnect);
                }
                // 3 = AlarmSeverity::Invalid; status 0 = LINK alarm
                // (downstream client cannot tell why upstream is gone,
                // only that it is — INVALID severity is the closest
                // EPICS alarm to "channel disconnected").
                let _ = db_clone.post_alarm(&name, 3, 0).await;

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
                    Err(_) => {
                        // Stay in Disconnect; another iteration of the
                        // outer loop will retry after the next sleep.
                        continue;
                    }
                }
            }
        });

        self.subs.lock().insert(
            upstream_name.to_string(),
            UpstreamSubscription {
                channel,
                task,
                asg,
                asl,
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
    pub async fn unsubscribe(&self, upstream_name: &str) {
        let removed = self.subs.lock().remove(upstream_name);
        if let Some(sub) = removed {
            sub.task.abort();
        }
        // Best-effort: if the PV is gone already (concurrent reload
        // path) `remove_simple_pv` returns None; we don't care.
        let _ = self.shadow_db.remove_simple_pv(upstream_name).await;
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

    /// ASG/ASL recorded for `upstream_name` at subscription time, if
    /// any. Used by tests + diagnostics.
    pub fn asg_for(&self, upstream_name: &str) -> Option<(Option<String>, i32)> {
        self.subs
            .lock()
            .get(upstream_name)
            .map(|s| (s.asg.clone(), s.asl))
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

/// Build the [`WriteHook`] closure for one upstream PV. Called once
/// per `ensure_subscribed`; the resulting `Arc<dyn Fn …>` is installed
/// on the shadow `ProcessVariable` so every client `caput` runs this
/// pipeline.
///
/// Pipeline (matches C ca-gateway `gatePvData::putCB` ordering):
/// 1. Read-only mode → reject + record stat + putlog
/// 2. Host-based DENY (pvlist `FROM host`) → reject + putlog
/// 3. ACF `can_write(asg, asl, user, host)` → reject + putlog
/// 4. Forward `caput` to upstream via the shared channel
/// 5. Putlog the outcome (Ok/Failed) and bump put-count stat
fn build_write_hook(
    pv_name: String,
    channel: Arc<CaChannel>,
    asg: Option<String>,
    asl: i32,
    env: WriteHookEnv,
) -> WriteHook {
    Arc::new(move |new_value: EpicsValue, ctx: WriteContext| {
        let pv_name = pv_name.clone();
        let channel = channel.clone();
        let asg = asg.clone();
        let env = env.clone();
        Box::pin(async move {
            // Bound the audit-log value so a client putting a 1M
            // element waveform doesn't allocate a 25MB String per
            // put and write a multi-megabyte putlog line. 256 chars
            // is enough for scalars, NTScalar, and a leading slice
            // of array values; full fidelity would belong in a
            // separate binary trace if ever needed.
            let value_str = format_value_for_audit(&new_value, 256);

            // 1. read-only mode — gateway-wide flag.
            if env.read_only {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str).await;
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
                log_denial(&env, &ctx, &pv_name, &value_str).await;
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
                log_denial(&env, &ctx, &pv_name, &value_str).await;
                return Err(CaError::ReadOnlyField(format!(
                    "{pv_name} (no client identity)"
                )));
            }
            let asg_ref = asg.as_deref().unwrap_or("DEFAULT");
            if !access.can_write(asg_ref, asl, &ctx.user, &ctx.host) {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str).await;
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
                    .log(&ctx.user, &ctx.host, &pv_name, &value_str, outcome)
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

/// Helper: emit a single `Denied` putlog line. Called from each
/// rejection branch in the WriteHook so the structure is uniform
/// (timestamp, user@host, pv, value, DENIED). Errors from the log
/// write itself are surfaced via `tracing` (debounced via target)
/// so a disk-full putlog doesn't silently disappear the audit
/// trail.
async fn log_denial(env: &WriteHookEnv, ctx: &WriteContext, pv: &str, value: &str) {
    if let Some(pl) = &env.putlog
        && let Err(e) = pl
            .log(&ctx.user, &ctx.host, pv, value, PutOutcome::Denied)
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
    // B-G16: bound the formatted-string allocation BEFORE running
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

    fn dummy_env() -> WriteHookEnv {
        WriteHookEnv {
            read_only: false,
            access: Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all())),
            pvlist: Arc::new(ArcSwap::from_pointee(PvList::new())),
            putlog: None,
            stats: Arc::new(Stats::new("gw:".into())),
            beacon_anomaly: Arc::new(crate::ca_gateway::beacon::BeaconAnomaly::new()),
        }
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
            beacon_anomaly: env.beacon_anomaly.clone(),
        })
        .await;
        assert!(mgr.is_ok());

        let mgr = mgr.unwrap();
        assert_eq!(mgr.subscription_count(), 0);
        assert!(!mgr.is_subscribed("ANY"));
    }

    #[test]
    fn _entry_imports() {
        // Sanity check that the cache types are in scope
        let _ = super::super::cache::GwPvEntry::new_connecting("X");
    }
}
