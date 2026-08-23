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
//! The monitor-forwarding task wraps `channel.subscribe_with_mask_autosize(..)`
//! (autosize wire count=0 + the configured `-mask` event mask) in an
//! exponential-backoff retry loop so a transient upstream disconnect
//! does not strand the cache entry forever (the entry's `cached`
//! snapshot would otherwise be served indefinitely while no further
//! events arrive). On terminal failure the entry transitions to the
//! `Disconnect` state, which the cleanup tick eventually evicts.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - `cached_old_for_audit_boundaries` runs and
// passes in the feature-ON suite (it touches the cache only, never the upstream
// client). The other twelve take gate (3); see the comment on
// `manager_construct`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use epics_base_rs::error::CaError;
use epics_base_rs::runtime::task::TaskHandle;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{WriteContext, WriteHook};
use epics_base_rs::server::snapshot::DbrClass;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};
use epics_ca_rs::client::{CaChannel, CaClient, EventWatcher, MonitorHandle};
use epics_ca_rs::protocol::DBE_PROPERTY;
use epics_ca_rs::server::AccessRightsNotifier;
use tokio::sync::RwLock;

use crate::error::{BridgeError, BridgeResult};

use super::access::AccessConfig;
use super::cache::{PvCache, PvState};
use super::putlog::{PutLog, PutLogLine, PutLogScope, PutOutcome};
use super::pvlist::{PolicyHost, PvList};
use super::server::CacheMode;
use super::stats::Stats;

/// Build a zero-initialised [`EpicsValue`] of the upstream channel's
/// native field type and element count.
///
/// Used as the shadow PV's initial value when the best-effort DBR
/// negotiation GET fails or times out. The shadow PV must still
/// advertise the upstream's *native* DBF type and element count at
/// `CREATE_CHANNEL`: C ca-gateway reads `ca_field_type(chid)` /
/// `ca_element_count(chid)` straight from the connected channel's CA
/// connection metadata (gatePv.cc:1241-1314, gatePv.h:139-142),
/// independent of any value GET succeeding. A connected upstream whose
/// first read is slow or denied must not be exposed downstream as
/// `DBF_DOUBLE/1`; the create-channel reply derives its type and count
/// from `value.dbr_type()` / `value.count()`, so giving the placeholder
/// the right variant and length makes both correct by construction. The
/// first monitor event overwrites the value, but a client that sizes its
/// buffer from the create reply in that window now sees the real type.
fn native_placeholder(native_type: DbFieldType, element_count: u32) -> EpicsValue {
    let n = element_count.max(1) as usize;
    let scalar = element_count <= 1;
    match native_type {
        DbFieldType::String if scalar => EpicsValue::String(PvString::new()),
        DbFieldType::String => EpicsValue::StringArray(vec![PvString::new(); n]),
        DbFieldType::Short if scalar => EpicsValue::Short(0),
        DbFieldType::Short => EpicsValue::ShortArray(vec![0; n]),
        DbFieldType::Float if scalar => EpicsValue::Float(0.0),
        DbFieldType::Float => EpicsValue::FloatArray(vec![0.0; n]),
        DbFieldType::Enum if scalar => EpicsValue::Enum(0),
        DbFieldType::Enum => EpicsValue::EnumArray(vec![0; n]),
        DbFieldType::Char if scalar => EpicsValue::Char(0),
        DbFieldType::Char => EpicsValue::CharArray(vec![0; n]),
        DbFieldType::Long if scalar => EpicsValue::Long(0),
        DbFieldType::Long => EpicsValue::LongArray(vec![0; n]),
        DbFieldType::Double if scalar => EpicsValue::Double(0.0),
        DbFieldType::Double => EpicsValue::DoubleArray(vec![0.0; n]),
        // CA wire types 7/8 do not exist: a CA upstream channel never
        // reports Int64/UInt64 natively (they are internal record types
        // that travel over CA as DBR_DOUBLE). Mirror that DBR mapping —
        // their `dbr_type()` is `Double`, so the advertised create-channel
        // type stays DBF_DOUBLE even on this unreachable branch.
        DbFieldType::Int64 if scalar => EpicsValue::Int64(0),
        DbFieldType::Int64 => EpicsValue::Int64Array(vec![0; n]),
        DbFieldType::UInt64 if scalar => EpicsValue::UInt64(0),
        DbFieldType::UInt64 => EpicsValue::UInt64Array(vec![0; n]),
        // Like Int64/UInt64, DBF_USHORT/DBF_ULONG never travel natively over
        // CA (they promote to DBR_LONG / DBR_DOUBLE), so these branches are
        // unreachable for a CA upstream; mapped for completeness.
        DbFieldType::UShort if scalar => EpicsValue::UShort(0),
        DbFieldType::UShort => EpicsValue::UShortArray(vec![0; n]),
        DbFieldType::ULong if scalar => EpicsValue::ULong(0),
        DbFieldType::ULong => EpicsValue::ULongArray(vec![0; n]),
        // DBF_UCHAR promotes to DBR_CHAR over CA (db_convert.h), so a CA
        // upstream reports it as DBF_CHAR, not DBF_UCHAR — this branch is
        // unreachable for a CA upstream but mapped for completeness. Its
        // `dbr_type()` is `Char`, keeping the advertised create-channel type
        // DBF_CHAR on this branch.
        DbFieldType::UChar if scalar => EpicsValue::UChar(0),
        DbFieldType::UChar => EpicsValue::UCharArray(vec![0; n]),
    }
}

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
    /// The monitor-forwarding task. `Some` whenever an upstream
    /// subscription is live: always in [`CacheMode::Cached`], and in
    /// [`CacheMode::NoCache`] only while ≥1 downstream client is
    /// monitoring (created by [`UpstreamManager::ensure_monitor`], torn
    /// down by [`UpstreamManager::release_monitor`]). `None` for a
    /// no-cache PV with no current monitor interest — GETs are still
    /// served via the shadow PV's read hook (fresh upstream fetch), no
    /// persistent subscription needed.
    task: Option<TaskHandle<()>>,
    /// The property-monitor-forwarding task — the distinct upstream
    /// `DBE_PROPERTY` subscription that refreshes the shadow PV's
    /// display/control/enum metadata and fans `DBE_PROPERTY` events out
    /// downstream. `Some` whenever it is live: always in
    /// [`CacheMode::Cached`] (spawned eagerly alongside [`Self::task`]),
    /// and in [`CacheMode::NoCache`] only while ≥1 downstream client holds
    /// a `DBE_PROPERTY` monitor (created by
    /// [`UpstreamManager::ensure_prop_monitor`], torn down by
    /// [`UpstreamManager::release_prop_monitor`]). `None` otherwise.
    /// Mirrors C ca-gateway's separate `pv->propMonitor()`
    /// (`gatePv.cc:1705`, `:1749-1752`).
    prop_task: Option<TaskHandle<()>>,
    /// Whether the connect-time DBR_CTRL metadata seed actually landed.
    /// When `false` (the 500 ms seed timed out or errored), a
    /// [`Self::prop_task`] spawned later (the no-cache lazy path in
    /// [`UpstreamManager::ensure_prop_monitor`]) must NOT skip its first
    /// DBE_PROPERTY event — that event carries the full metadata and seeds
    /// what the connect-time get missed. The cached path reads this from the
    /// local seed outcome directly; storing it here lets the lazy path honor
    /// the same invariant. See [`UpstreamManager::spawn_prop_forward_task`].
    seed_succeeded: bool,
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
    /// Which writes the put log records. `TrapWrite` (default) reproduces
    /// the C contract — only granted writes whose matched rule carries
    /// `TRAPWRITE` (`gateVc.cc:236`); `AllWrites` logs every attempt.
    putlog_scope: PutLogScope,
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
    /// Put-log scope: `TrapWrite` (C contract) or `AllWrites` (broader
    /// audit). Threaded from `GatewayConfig::putlog_scope`.
    pub putlog_scope: PutLogScope,
    pub stats: Arc<Stats>,
    pub read_only: bool,
    /// Cache mode (C `cacheMode` / `-no_cache`). In
    /// [`CacheMode::NoCache`] `ensure_subscribed` installs a read hook
    /// that forwards each downstream GET to upstream and defers the
    /// upstream monitor until [`UpstreamManager::ensure_monitor`] is
    /// called on the first downstream subscription.
    pub cache_mode: CacheMode,
    /// Upstream connect-timeout budget for lazy search resolution.
    /// Threaded from `GatewayConfig::timeouts.connect_timeout` so the
    /// CLI/API connect-timeout knob also governs first-search resolution,
    /// not just cache cleanup (a single connect-timeout owner).
    pub connect_timeout: Duration,
    /// Upstream monitor event mask (C `-mask`). Threaded from
    /// `GatewayConfig::event_mask` so every upstream subscription requests
    /// exactly the configured `DBE_*` bits (default `DBE_VALUE|DBE_ALARM`)
    /// instead of the `CaChannel::subscribe()` default that adds
    /// `DBE_LOG`.
    pub event_mask: u16,
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
    /// Bound on the best-effort connect-time DBR_CTRL metadata seed in
    /// `ensure_subscribed` (default 500 ms). A slow/denied metadata read
    /// must not stall the subscription; when it exceeds this budget the seed
    /// is abandoned and the property monitor's first event seeds instead
    /// (see `seed_succeeded`). Not a public knob — tests set it to
    /// `Duration::ZERO` to deterministically exercise the seed-miss recovery.
    metadata_seed_timeout: Duration,
    /// Cache mode (C `cacheMode` / `-no_cache`). Gates whether
    /// `ensure_subscribed` holds a persistent upstream monitor and serves
    /// GETs from the shadow snapshot (`Cached`) or forwards each GET to
    /// upstream and creates the monitor lazily per downstream-monitor
    /// interest (`NoCache`).
    cache_mode: CacheMode,
    /// Upstream monitor event mask (C `-mask`, default
    /// `DBE_VALUE|DBE_ALARM`). Both the initial `ensure_subscribed`
    /// subscribe and the reconnect re-subscribe inside the forwarding
    /// task pass this mask, so the gateway never requests `DBE_LOG`
    /// unless `-mask l` was configured. Mirrors ca-gateway passing
    /// `GR->eventMask()` to `ca_create_subscription` (gatePv.cc:771-774).
    event_mask: u16,
    /// Detachable handle that re-pushes `CA_PROTO_ACCESS_RIGHTS` to
    /// already-connected downstream clients. Installed via
    /// [`Self::install_access_notifier`] once the downstream `CaServer`
    /// exists (the manager is built first), so it is wrapped in an
    /// `ArcSwapOption` for lock-free interior mutability. Fired whenever a
    /// gateway-side change flips a channel's computed downstream access —
    /// an upstream IOC write-access event (`on_access_rights_change`) or an
    /// `.acf`/`.pvlist` reload re-resolving per-PV ACLs
    /// ([`Self::notify_downstream_access_change`]). Mirrors C ca-gateway
    /// `postAccessRights` / `gateChan::resetAsClient` (gateVc.cc:170-199,
    /// 1624-1638) and RSRV `asComputeAllAsg`.
    access_notifier: Arc<ArcSwapOption<AccessRightsNotifier>>,
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
                putlog_scope: cfg.putlog_scope,
                cache: cfg.cache,
                stats: cfg.stats,
                beacon_anomaly: cfg.beacon_anomaly,
            },
            subs: parking_lot::Mutex::new(HashMap::new()),
            pending: parking_lot::Mutex::new(HashMap::new()),
            connect_timeout: cfg.connect_timeout,
            metadata_seed_timeout: Duration::from_millis(500),
            cache_mode: cfg.cache_mode,
            event_mask: cfg.event_mask,
            access_notifier: Arc::new(ArcSwapOption::empty()),
        })
    }

    /// Install the downstream access-rights notifier.
    ///
    /// The manager is constructed before the downstream `CaServer` exists,
    /// so the gateway snapshots the server's [`AccessRightsNotifier`] during
    /// build (after the server is created, before `run` consumes it) and
    /// installs it here. Once installed, gateway-side access-state changes
    /// re-push `CA_PROTO_ACCESS_RIGHTS` to already-connected clients instead
    /// of silently updating only the hook flag.
    pub fn install_access_notifier(&self, notifier: AccessRightsNotifier) {
        self.access_notifier.store(Some(Arc::new(notifier)));
    }

    /// Fire the installed downstream access-rights notifier, if any.
    ///
    /// Prompts every connected downstream client to re-run its per-channel
    /// access computation (the gateway's access hook, which ANDs the live
    /// per-PV ACL with the upstream write bit) and re-push
    /// `CA_PROTO_ACCESS_RIGHTS` only where the computed level changed. Called
    /// by the `AS`/`PVL` reload owner after re-resolving every still-admitted
    /// PV's ACL (C `gateServer::newAs` → `asComputeAllAsg`,
    /// gateServer.cc:603-630). A no-op until [`Self::install_access_notifier`]
    /// has run.
    pub fn notify_downstream_access_change(&self) {
        if let Some(notifier) = self.access_notifier.load_full() {
            notifier.notify();
        }
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
        // PV's first registered value matches upstream's native type.
        // channel.get() returns (DbFieldType, EpicsValue) only — no
        // DBR_CTRL_* metadata (units/precision/limits). A DBR_CTRL GET +
        // DBE_PROPERTY subscription is needed so downstream DBR_CTRL_*
        // and DBR_GR_* reads return real metadata (gatePv.cc:930-934,
        // gatePv.cc:858-862).
        //
        // When the GET fails or times out, fall back to a placeholder
        // whose CA native type AND element count come from the *connected*
        // channel's CA metadata (`ca_field_type`/`ca_element_count`), not
        // a hardcoded `Double(0.0)`. The channel connected above
        // (`wait_connected`), so this metadata is a cached snapshot read
        // with no round-trip. Without it a slow or denied first read
        // advertised the shadow PV as DBF_DOUBLE/1 to clients that connect
        // in the window before the first monitor event (see
        // `native_placeholder`). Built lazily so the success path never
        // allocates a throwaway array for a large waveform.
        let make_placeholder = || match (channel.native_field_type(), channel.element_count()) {
            (Ok(dbf), Ok(n)) => native_placeholder(dbf, n),
            // Pathological: metadata unreadable despite a connected
            // channel. Keep the old scalar-double fallback rather than
            // failing the subscription outright.
            _ => EpicsValue::Double(0.0),
        };
        let initial_value =
            match epics_base_rs::runtime::task::timeout(Duration::from_millis(500), channel.get())
                .await
            {
                Ok(Ok((_dbf, v))) => v,
                Ok(Err(e)) => {
                    tracing::info!(
                        pv = upstream_name,
                        error = %e,
                        "ca-gateway-rs: DBR negotiation get failed; \
                         using upstream native type/count placeholder"
                    );
                    make_placeholder()
                }
                Err(_) => {
                    tracing::info!(
                        pv = upstream_name,
                        "ca-gateway-rs: DBR negotiation get timed out; \
                     using upstream native type/count placeholder"
                    );
                    make_placeholder()
                }
            };

        // read initial upstream read+write access and create the flags
        // the access hook AND write-hook closures share. `channel.info()`
        // reads a cached snapshot — no round-trip — and succeeds here
        // because `channel.get()` above already waited for connection.
        // Defaults to true (permissive) on the rare timeout/error path so
        // we match the C gateway's connect-time behaviour (activate() reads
        // ca_read_access/ca_write_access only after the channel is
        // operational). One `info()` snapshot feeds both flags so read and
        // write can't come from inconsistent reads.
        let (upstream_read_init, upstream_write_init) = match channel.info().await {
            Ok(i) => (i.access_rights.read, i.access_rights.write),
            Err(_) => (true, true),
        };
        let upstream_write = Arc::new(AtomicBool::new(upstream_write_init));
        let upstream_read = Arc::new(AtomicBool::new(upstream_read_init));

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
            upstream_read.clone(),
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
        // In no-cache mode (C `-no_cache`), serve each downstream GET
        // from a FRESH upstream fetch instead of the shadow snapshot:
        // install a read hook that forwards to the shared upstream
        // channel with a metadata-bearing `get_with_metadata(DbrClass::Time)`.
        // C `-no_cache` reads issue `ca_array_get_callback(eventType(), ...)`
        // with `eventType()` a `DBR_TIME_*` class, and `getTimeCB` decodes
        // the event's status/severity/timestamp into `setEventData`
        // (`gatePv.cc:976`, `:1789-1794`); requesting the `DBR_TIME_*`
        // variant here makes the fresh value travel WITH its upstream
        // alarm/timestamp, so the server read path never grafts a fresh
        // value onto a stale/default shadow snapshot. It captures the same
        // `Arc<CaChannel>` the write hook reuses, so a GET costs one
        // upstream get with no fresh CREATE_CHAN. The hook lands ATOMICALLY
        // with registration (`add_pv_with_hooks_full`), closing the same
        // CREATE_CHAN+READ race the write/access hooks close. Cached mode
        // installs no read hook — GETs serve the monitor-fed shadow value
        // as before. Mirrors `gateVc.cc:1361-1369` forwarding the read to
        // the IOC.
        let read_hook: Option<epics_base_rs::server::pv::ReadHook> =
            if self.cache_mode.is_no_cache() {
                let ch = channel.clone();
                Some(Arc::new(move || {
                    let ch = ch.clone();
                    Box::pin(async move { ch.get_with_metadata(DbrClass::Time).await })
                }))
            } else {
                None
            };
        self.shadow_db.remove_simple_pv(served_name).await;
        if let Err(e) = self
            .shadow_db
            .add_pv_with_hooks_full(
                served_name,
                initial_value,
                hook,
                Some(access_hook),
                read_hook,
            )
            .await
        {
            return Err(BridgeError::PutRejected(format!(
                "shadow PV register failed: {e}"
            )));
        }

        // Seed the shadow PV's DBR_CTRL_* metadata from an initial control
        // get so a downstream DBR_CTRL_*/DBR_GR_* read returns the real
        // units / precision / limits / enum-labels instead of zeroed ones —
        // even before any property *change* occurs. Best-effort and bounded
        // (500 ms): a slow or denied metadata read must not fail the
        // subscription, since the value path is already wired above. When it
        // does NOT seed (timeout or error), `seed_succeeded` stays false and
        // the property monitor's FIRST event is consumed to seed instead of
        // being skipped as a redundant confirmation — so a slow initial get
        // never leaves metadata zeroed. Mirrors C `gatePvData::getCB`
        // decoding the initial control get through `runDataCB` →
        // `vc->setPvData(dd)` and only THEN enabling `propMonitor()`
        // (gatePv.cc:1688-1705); C sequences the monitor after the get, we
        // instead make the first-event skip conditional on the seed outcome.
        let seed_succeeded = if self.metadata_seed_timeout.is_zero() {
            // A zero seed budget means "spend no time on the connect-time
            // seed" — skip it entirely and let the property monitor's first
            // event seed. Deterministic (no timeout race), and the recovery
            // path tests exercise the seed-miss branch this way, since an
            // in-process upstream's CTRL get resolves from cached channel
            // metadata too fast to time out.
            false
        } else {
            match epics_base_rs::runtime::task::timeout(
                self.metadata_seed_timeout,
                channel.get_with_metadata(DbrClass::Ctrl),
            )
            .await
            {
                Ok(Ok(meta_snap)) => match self
                    .shadow_db
                    .set_pv_metadata(served_name, &meta_snap)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::debug!(
                            pv = upstream_name,
                            served = served_name,
                            error = %e,
                            "ca-gateway-rs: initial CTRL metadata seed skipped; \
                             property monitor's first event will seed it"
                        );
                        false
                    }
                },
                Ok(Err(e)) => {
                    tracing::info!(
                        pv = upstream_name,
                        error = %e,
                        "ca-gateway-rs: initial CTRL metadata get failed; \
                         property monitor's first event will seed it"
                    );
                    false
                }
                Err(_) => {
                    tracing::info!(
                        pv = upstream_name,
                        "ca-gateway-rs: initial CTRL metadata get timed out; \
                         property monitor's first event will seed it"
                    );
                    false
                }
            }
        };

        // Subscribe the persistent upstream monitor — CACHED mode only.
        //
        // Cached: subscribe now. Autosize (wire count=0) + the configured
        // `-mask` event mask mirrors ca-gateway's
        // `ca_create_subscription(eventType(), 0, chID, GR->eventMask(), ...)`
        // (gatePv.cc:765-774) — count 0 so the IOC reports each event at
        // the record's CURRENT element count, and the configured mask
        // (default DBE_VALUE|DBE_ALARM) rather than the CaChannel default
        // that also requests DBE_LOG. `deadband` is 0.0 — ca-gateway
        // applies no client-side deadband. On failure we MUST remove the
        // just-added shadow PV — otherwise it lingers in `simple_pvs`
        // with a hook pointing at a dead channel and the next search
        // resolves it without re-running the resolver, stuck.
        //
        // No-cache: hold NO persistent monitor. Mark the entry existent
        // (Inactive) so the search resolver advertises it and the
        // connect-timeout reaper does not evict it (no monitor event will
        // arrive to flip Connecting→Inactive). The upstream monitor is
        // created lazily by `ensure_monitor` on the first downstream
        // subscription (C `getCB` no-cache `needPosting()` gate,
        // gatePv.cc:1737-1753); GETs meanwhile forward fresh via the read
        // hook installed above.
        let initial_monitor: Option<MonitorHandle> = if self.cache_mode.is_no_cache() {
            if let Some(entry) = self.cache.read().await.get(served_name) {
                entry.write().await.set_state(PvState::Inactive);
            }
            None
        } else {
            match channel
                .subscribe_with_mask_autosize(0.0, self.event_mask)
                .await
            {
                Ok(m) => Some(m),
                Err(e) => {
                    self.shadow_db.remove_simple_pv(served_name).await;
                    return Err(BridgeError::PutRejected(format!("subscribe failed: {e}")));
                }
            }
        };

        // keep upstream_write up-to-date when the IOC's write-
        // access changes (e.g. ASG protection lockout). The C gateway's
        // accessCB (gatePv.cc:1851-1852) calls setWriteAccess +
        // postAccessRights, and postAccessRights loops the downstream
        // channels posting CA_PROTO_ACCESS_RIGHTS (gateVc.cc:1624-1638).
        // We mirror that second half: when the upstream write bit actually
        // flips, fire the downstream access-rights notifier so connected
        // clients re-run the access hook (which ANDs the per-PV ACL with
        // this flag) and re-push CA_PROTO_ACCESS_RIGHTS where the computed
        // level changed. Gated on an observed transition so a no-op
        // callback does not wake every client; the downstream side applies
        // the further `oldaccess != access` filter.
        let access_rights_watcher = channel.on_access_rights_change({
            let write_flag = upstream_write.clone();
            let read_flag = upstream_read.clone();
            let notifier = self.access_notifier.clone();
            move |rights| {
                // C accessCB (gatePv.cc:1851-1852) updates BOTH read and
                // write from ca_read_access/ca_write_access; postAccessRights
                // re-pushes when the computed level changed on either bit.
                // Track both flags and wake downstream clients if either
                // flipped.
                let prev_w = write_flag.swap(rights.write, Ordering::Relaxed);
                let prev_r = read_flag.swap(rights.read, Ordering::Relaxed);
                if prev_w != rights.write || prev_r != rights.read {
                    if let Some(n) = notifier.load_full() {
                        n.notify();
                    }
                }
            }
        });

        // Spawn the monitor-forwarding task. CACHED: spawn now, seeded
        // with the monitor subscribed above. NO-CACHE: `initial_monitor`
        // is `None`, so no task spawns here — `ensure_monitor` creates it
        // (self-subscribing) on the first downstream monitor, and
        // `release_monitor` aborts it when the last monitor leaves.
        let task = initial_monitor
            .map(|monitor| self.spawn_forward_task(served_name, channel.clone(), Some(monitor)));

        // Property monitor: CACHED spawns it eagerly alongside the value
        // monitor (C `getCB` [CACHE] enables `propMonitor()` unconditionally,
        // gatePv.cc:1705). NO-CACHE leaves it `None` here —
        // `ensure_prop_monitor` creates it lazily on the first downstream
        // DBE_PROPERTY monitor, matching C `getCB` [NO_CACHE] gating
        // `propMonitor()` on `needPosting() && client_mask == DBE_PROPERTY`
        // (gatePv.cc:1749-1752).
        let prop_task = if self.cache_mode.is_no_cache() {
            None
        } else {
            Some(self.spawn_prop_forward_task(served_name, channel.clone(), seed_succeeded))
        };

        // subscription dedup keys on `served_name` so a
        // repeat search for the same alias is a no-op (fast path above)
        // and a `.pvlist` reload prunes by the served name it admits.
        self.subs.lock().insert(
            served_name.to_string(),
            UpstreamSubscription {
                channel,
                task,
                prop_task,
                seed_succeeded,
                acl,
                _access_rights_watcher: access_rights_watcher,
            },
        );
        Ok(())
    }

    /// Spawn the upstream monitor-forwarding task for `served_name`.
    ///
    /// The single owner of one PV's upstream-subscription lifetime:
    /// subscribes (autosize count=0 + configured `-mask` event mask),
    /// forwards every event into the gateway cache + shadow PvDatabase,
    /// surfaces an INVALID alarm on upstream disconnect, and re-subscribes
    /// with exponential backoff. Used by `ensure_subscribed` (cached
    /// mode, seeded with `initial_monitor`) and by `ensure_monitor`
    /// (no-cache, `initial_monitor = None` → the task subscribes itself).
    ///
    /// Does NOT borrow the channel, so the direct put()/get() path uses
    /// the same channel without contention. `release_monitor` (no-cache)
    /// aborts the returned handle; aborting only drops the in-memory
    /// monitor stream and stops the loop — no external truth changes and
    /// the last cached value stays put, so no finalizer is needed.
    fn spawn_forward_task(
        &self,
        served_name: &str,
        channel: Arc<CaChannel>,
        initial_monitor: Option<MonitorHandle>,
    ) -> TaskHandle<()> {
        let cache_clone = self.cache.clone();
        let db_clone = self.shadow_db.clone();
        let stats_for_task = self.write_env.stats.clone();
        let beacon_anomaly_for_task = self.write_env.beacon_anomaly.clone();
        // The reconnect / first-subscribe re-arm must use the same
        // autosize (wire count=0) + configured `-mask` event mask as the
        // seeded subscribe, not the DBE_LOG-bearing CaChannel default.
        let event_mask = self.event_mask;
        // the forwarding task addresses the cache entry, shadow PV, and
        // alarm post by `served_name` — the same key the shadow PV and
        // cache were registered under above.
        let name = served_name.to_string();
        epics_base_rs::runtime::task::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            // Seed the first iteration with the monitor the caller
            // subscribed (cached mode); `None` (no-cache) subscribes at
            // the top of the loop. Every reconnect re-subscribes the same
            // way, so there is ONE subscribe site, not two.
            let mut next_monitor = initial_monitor;
            loop {
                let mut monitor = match next_monitor.take() {
                    Some(m) => m,
                    None => match channel.subscribe_with_mask_autosize(0.0, event_mask).await {
                        Ok(m) => m,
                        // `CaError::Shutdown` means the CA client
                        // coordinator is gone — `subscribe()` will never
                        // succeed again, so retrying just spins at the
                        // backoff cap forever. Exit cleanly; the cache
                        // entry stays in Disconnect and the next search
                        // resolver pass re-creates the channel against a
                        // live client.
                        Err(CaError::Shutdown) => {
                            tracing::debug!(
                                pv = %name,
                                "ca-gateway-rs: CA coordinator gone, \
                                 stopping upstream monitor task"
                            );
                            return;
                        }
                        Err(_) => {
                            // Transient subscribe failure (e.g. upstream
                            // still reconnecting). Back off and retry,
                            // unless the cache entry was evicted meanwhile
                            // (nobody cares anymore).
                            if cache_clone.read().await.get(&name).is_none() {
                                return;
                            }
                            epics_base_rs::runtime::task::sleep(backoff).await;
                            backoff = std::cmp::min(backoff * 2, max_backoff);
                            continue;
                        }
                    },
                };
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
                let _ = db_clone.post_alarm(
                    &name,
                    3,
                    epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                );

                // Back off, then loop: the top of the loop re-subscribes
                // (the single subscribe site). Bail out if the cache
                // entry has been evicted (nobody cares anymore). The
                // CaChannel itself drives reconnect under the hood; this
                // loop merely re-arms the monitor stream once it is back.
                epics_base_rs::runtime::task::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, max_backoff);
                if cache_clone.read().await.get(&name).is_none() {
                    return;
                }
            }
        })
    }

    /// Spawn the upstream property-monitor-forwarding task for
    /// `served_name` — the distinct `DBE_PROPERTY` subscription that keeps
    /// the shadow PV's display/control/enum metadata in step with upstream
    /// and fans property events out to downstream DBE_PROPERTY monitors.
    ///
    /// The subscription requests the `DBE_PROPERTY` mask (autosize count=0)
    /// and acts purely as a *trigger*: its delivered payload is the value
    /// type and carries no metadata, so each event drives a fresh
    /// `get_with_metadata(DbrClass::Ctrl)` to read the new
    /// limits/precision/enum-labels. DBE_PROPERTY events are rare (metadata
    /// seldom changes), so the extra control get per event is cheap. The
    /// decoded CTRL snapshot carries the upstream status/severity and an
    /// undefined (control-DBR) timestamp; `post_pv_property` refreshes the
    /// shadow metadata and posts that snapshot to downstream property
    /// subscribers WITHOUT inventing a wall-clock time. Mirrors C
    /// `gatePvData::propEventCB` (gatePv.cc:1534-1607): the first event is
    /// the subscription's initial-state confirmation (C `propGetPending()` /
    /// `markPropNoGetPending`, gatePv.cc:1564-1568). It is skipped as
    /// redundant ONLY when `seed_succeeded` — i.e. the connect-time control
    /// get in `ensure_subscribed` already seeded metadata. If that seed did
    /// NOT land (slow/failed 500 ms get), the first event is instead consumed
    /// to seed: it drives the same `get_with_metadata(DbrClass::Ctrl)` the
    /// seed would have, closing the window where a stable PV's metadata would
    /// otherwise stay zeroed until a *second* property change that may never
    /// come. Every later event (including a reconnect's initial event, which
    /// may carry metadata that changed during the outage) refreshes
    /// `setPvData` and posts `propertyEventMask()`.
    ///
    /// Re-subscribes with exponential backoff on upstream disconnect, exits
    /// cleanly on `CaError::Shutdown` or cache eviction, and changes no
    /// external truth (aborting only drops the in-memory monitor stream and
    /// leaves the last-installed metadata in place), so
    /// `release_prop_monitor` may abort the returned handle with no
    /// finalizer — symmetric to `spawn_forward_task`. Unlike the value task
    /// it surfaces no disconnect alarm: the value task already owns the
    /// downstream disconnect signal; this task only re-arms its metadata
    /// stream.
    fn spawn_prop_forward_task(
        &self,
        served_name: &str,
        channel: Arc<CaChannel>,
        seed_succeeded: bool,
    ) -> TaskHandle<()> {
        let cache_clone = self.cache.clone();
        let db_clone = self.shadow_db.clone();
        let name = served_name.to_string();
        epics_base_rs::runtime::task::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            // The very first event of the FIRST subscription is the
            // initial-state confirmation. Skip it as redundant ONLY when the
            // connect-time control get already seeded metadata
            // (`seed_succeeded`) — mirrors C ignore-first-propEvent
            // (gatePv.cc:1564-1568), which is safe there because C enables
            // propMonitor() only after getCB seeds (gatePv.cc:1702-1705).
            // When the seed did NOT land (slow/failed 500 ms get), consume the
            // first event to seed instead — its DBR_CTRL payload carries the
            // full metadata, so metadata is never left zeroed. Tracked across
            // reconnects so a reconnect's initial event DOES refresh — it may
            // carry metadata that changed while upstream was gone.
            let mut first_event_seen = !seed_succeeded;
            loop {
                let mut monitor = match channel
                    .subscribe_with_mask_autosize(0.0, DBE_PROPERTY)
                    .await
                {
                    Ok(m) => m,
                    // Coordinator gone — retrying would spin forever; exit.
                    Err(CaError::Shutdown) => {
                        tracing::debug!(
                            pv = %name,
                            "ca-gateway-rs: CA coordinator gone, \
                             stopping upstream property monitor task"
                        );
                        return;
                    }
                    Err(_) => {
                        if cache_clone.read().await.get(&name).is_none() {
                            return;
                        }
                        epics_base_rs::runtime::task::sleep(backoff).await;
                        backoff = std::cmp::min(backoff * 2, max_backoff);
                        continue;
                    }
                };
                while let Some(result) = monitor.recv().await {
                    // The trigger payload is discarded; a decode error is a
                    // missed trigger, not fatal.
                    if result.is_err() {
                        continue;
                    }
                    if !first_event_seen {
                        first_event_seen = true;
                        continue;
                    }
                    // A property change fired: re-read the full control
                    // metadata (units/precision/limits/enum-labels + upstream
                    // status/severity) and refresh + fan out downstream.
                    match channel.get_with_metadata(DbrClass::Ctrl).await {
                        Ok(meta_snap) => {
                            if let Err(e) = db_clone.post_pv_property(&name, meta_snap).await {
                                tracing::debug!(
                                    pv = %name,
                                    error = %e,
                                    "ca-gateway-rs: shadow post_pv_property failed"
                                );
                            }
                        }
                        Err(e) => tracing::debug!(
                            pv = %name,
                            error = %e,
                            "ca-gateway-rs: property-event CTRL get failed; \
                             shadow metadata not refreshed this event"
                        ),
                    }
                    backoff = Duration::from_millis(250);
                }
                // Subscription closed (upstream disconnect). Back off, then
                // the top of the loop re-subscribes. Bail if the cache entry
                // was evicted (nobody cares anymore).
                epics_base_rs::runtime::task::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, max_backoff);
                if cache_clone.read().await.get(&name).is_none() {
                    return;
                }
            }
        })
    }

    /// No-cache: ensure the upstream monitor for `served_name` is live.
    ///
    /// Called by the connection-event owner when the FIRST downstream
    /// client opens a monitor (`EVENT_ADD`) on this PV. Spawns the
    /// forwarding task (which subscribes itself) iff no task is currently
    /// running for the subscription. Idempotent: a second concurrent
    /// monitor-open is a no-op because the task already exists.
    ///
    /// No-op in [`CacheMode::Cached`] — there the persistent monitor is
    /// created eagerly at `ensure_subscribed`, so it is always present.
    /// Mirrors C ca-gateway's no-cache `getCB` creating `pv->monitor()`
    /// only when `vc->needPosting()` (gatePv.cc:1737-1753).
    pub fn ensure_monitor(&self, served_name: &str) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        // Take the channel under the lock, then spawn outside the lock
        // (spawn is sync but keep the critical section minimal), then
        // store the handle under the lock — re-checking no peer raced a
        // task in between.
        let channel = {
            let subs = self.subs.lock();
            match subs.get(served_name) {
                Some(sub) if sub.task.is_none() => sub.channel.clone(),
                // No subscription (evicted) or a task already runs.
                _ => return,
            }
        };
        let handle = self.spawn_forward_task(served_name, channel, None);
        let mut subs = self.subs.lock();
        match subs.get_mut(served_name) {
            // Re-check: another caller may have raced a task in, or the
            // subscription may have been evicted, while we spawned.
            Some(sub) if sub.task.is_none() => sub.task = Some(handle),
            _ => handle.abort(),
        }
    }

    /// No-cache: drop the upstream monitor for `served_name`.
    ///
    /// Called by the connection-event owner when the LAST downstream
    /// monitor on this PV closes. Aborts the forwarding task; the shadow
    /// PV and upstream channel stay (GETs still forward fresh via the
    /// read hook). Idempotent: a no-op if no task is running.
    ///
    /// No-op in [`CacheMode::Cached`] — the persistent monitor must
    /// outlive any single downstream subscriber.
    pub fn release_monitor(&self, served_name: &str) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        let task = self
            .subs
            .lock()
            .get_mut(served_name)
            .and_then(|sub| sub.task.take());
        if let Some(task) = task {
            task.abort();
        }
    }

    /// No-cache: ensure the upstream property monitor for `served_name` is
    /// live.
    ///
    /// Called by the connection-event owner when the FIRST downstream
    /// client opens a `DBE_PROPERTY` monitor (`EVENT_ADD` with
    /// `mask & DBE_PROPERTY`) on this PV. Spawns the property-forwarding
    /// task iff none currently runs. Idempotent. No-op in
    /// [`CacheMode::Cached`] — there the property monitor is created
    /// eagerly at `ensure_subscribed`, so it is always present. Mirrors C
    /// no-cache `getCB`/`propEventCB` enabling `propMonitor()` only on
    /// `needPosting() && client_mask == DBE_PROPERTY` (gatePv.cc:1749-1752).
    pub fn ensure_prop_monitor(&self, served_name: &str) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        let (channel, seed_succeeded) = {
            let subs = self.subs.lock();
            match subs.get(served_name) {
                Some(sub) if sub.prop_task.is_none() => (sub.channel.clone(), sub.seed_succeeded),
                // No subscription (evicted) or a prop task already runs.
                _ => return,
            }
        };
        let handle = self.spawn_prop_forward_task(served_name, channel, seed_succeeded);
        let mut subs = self.subs.lock();
        match subs.get_mut(served_name) {
            // Re-check: another caller may have raced a prop task in, or the
            // subscription may have been evicted, while we spawned.
            Some(sub) if sub.prop_task.is_none() => sub.prop_task = Some(handle),
            _ => handle.abort(),
        }
    }

    /// No-cache: drop the upstream property monitor for `served_name`.
    ///
    /// Called by the connection-event owner when the LAST downstream
    /// `DBE_PROPERTY` monitor on this PV closes. Aborts the
    /// property-forwarding task; the shadow PV's last-installed metadata
    /// stays in place. Idempotent: a no-op if no prop task is running. No-op
    /// in [`CacheMode::Cached`] — the persistent property monitor must
    /// outlive any single downstream subscriber.
    pub fn release_prop_monitor(&self, served_name: &str) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        let task = self
            .subs
            .lock()
            .get_mut(served_name)
            .and_then(|sub| sub.prop_task.take());
        if let Some(task) = task {
            task.abort();
        }
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
            if let Some(task) = sub.task {
                task.abort();
            }
            if let Some(prop_task) = sub.prop_task {
                prop_task.abort();
            }
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
    /// This swaps only the per-PV ACL cell; it does not itself notify
    /// downstream clients. Runtime re-notification of *already-connected*
    /// clients (C `gateChan::resetAsClient` posting an access-rights event,
    /// gateVc.cc:170-199) is owned by the reload caller, which fires
    /// [`Self::notify_downstream_access_change`] once after re-resolving
    /// every still-admitted PV — one `asComputeAllAsg`-style recompute pass
    /// per reload rather than one broadcast per mutated PV.
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
            if let Some(task) = sub.task {
                task.abort();
            }
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
/// `upstream_write`/`upstream_read` mirror the upstream IOC's
/// `ca_write_access(chID)`/`ca_read_access(chID)`, set at connect time and
/// kept live by `on_access_rights_change`. The decisions are
/// `local_acf_write && upstream_write` and `local_acf_read && upstream_read`,
/// matching C `gateVcChan::writeAccess`/`readAccess` (gateVc.cc:341/326):
/// `asclient->writeAccess() && vc->writeAccess()` /
/// `asclient->readAccess() && vc->readAccess()`.
fn build_access_hook(
    acl: PvAclCell,
    access: Arc<ArcSwap<AccessConfig>>,
    upstream_write: Arc<AtomicBool>,
    upstream_read: Arc<AtomicBool>,
) -> epics_base_rs::server::pv::AccessHook {
    Arc::new(move |user: &str, host: &str| {
        let cfg = access.load();
        // Load the live `.pvlist` identity each call so a reloaded
        // ASG/ASL is enforced immediately.
        let pv_acl = acl.load();
        let asg_ref = pv_acl.asg.as_deref().unwrap_or("DEFAULT");
        let asl = pv_acl.asl;
        let local_read = cfg.can_read(asg_ref, asl, user, host);
        let read = local_read && upstream_read.load(Ordering::Relaxed);
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
/// 1. Read-only mode → reject (+ denial putlog only in `AllWrites` scope)
/// 2. Host-based DENY (pvlist `FROM host`) → reject (+ `AllWrites` putlog)
/// 3. ACF `can_write_trap(asg, asl, user, host)` → reject (+ `AllWrites`
///    putlog) on deny; on grant, carry the matched rule's `TRAPWRITE` mask
///    forward
/// 4. Forward `caput` to upstream via the shared channel
/// 5. Putlog per scope: `AllWrites` logs the outcome (Ok/Failed) for every
///    attempt; `TrapWrite` (C contract, `gateVc.cc:236`) logs only this
///    granted write's attempt and only when its rule carried `TRAPWRITE`.
///    Always bump the put-count stat on success.
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
            // value/old are logged ONLY on the opt-in `AllWrites`
            // (`--putlog-all`) audit line; the default C `TrapWrite` line
            // is `timestamp user@host pv` with no value and no old
            // (gateVc.cc:240). Compute them only for that scope so the
            // default path neither formats the value nor does the async
            // cache read for an `old=` field it will never write.
            let audit_data = env.putlog.is_some() && env.putlog_scope == PutLogScope::AllWrites;

            // Bound the audit-log value so a client putting a 1M
            // element waveform doesn't allocate a 25MB String per
            // put and write a multi-megabyte putlog line. 256 chars
            // is enough for scalars, NTScalar, and a leading slice
            // of array values; full fidelity would belong in a
            // separate binary trace if ever needed.
            let value_str = if audit_data {
                format_value_for_audit(&new_value, 256)
            } else {
                String::new()
            };

            // Prior cached value for the audit `old=` field. C ca-gateway
            // logs `vc->eventData()` — the virtual connection's cached
            // monitor value — as the put's `old` value
            // (gateResources.cc:486-492). Read once up-front, keyed by
            // `served_name` (the cache key, upstream.rs:313/481), so every
            // `AllWrites` log path (denial branches + forward outcome)
            // records the same pre-put value.
            let old_str = if audit_data {
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
            // The identity is the SOCKET, never `ctx.host` — that field is
            // the name the client claims in CA `HOST_NAME`, so using it here
            // let a client pick which DENY row applied to its own writes,
            // and disagreed with the search/create path
            // (`server.rs`) which has always used the peer address. C pins
            // this to the socket too, with `getClientHostName` commented out
            // (`gateServer.cc:1523-1530`). `PolicyHost` is constructible
            // only from a peer, so the two points cannot drift apart again.
            //
            // An unparseable peer is DENY: a blacklist cannot be shown not
            // to apply to a peer we cannot establish.
            let Some(policy_host) = PolicyHost::from_peer_str(&ctx.peer) else {
                tracing::warn!(
                    peer = %ctx.peer, pv = %pv_name,
                    "pvlist DENY FROM: peer address unparseable, refusing the put"
                );
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::PutDisabled(format!(
                    "{pv_name} (peer address unavailable for pvlist evaluation)"
                )));
            };
            if pvlist.is_host_denied(&pv_name, &policy_host) {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(CaError::PutDisabled(format!(
                    "{pv_name} (host {} denied by pvlist)",
                    policy_host.as_str()
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
            // Keep the matched rule's TRAPWRITE mask alongside the
            // allow/deny so step 5 can reproduce C's trap-scoped put log
            // instead of re-deriving (and discarding) it. `permit.trap`
            // is false for any denied write (base-rs `NoAccess` ⇒ no mask).
            let permit = access.can_write_trap(asg_ref, asl, &ctx.user, &ctx.host);
            if !permit.allowed {
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

            // 5. Putlog + stats. The single audit owner
            // ([`emit_putlog`]/[`putlog_outcome`]) decides whether and how
            // this forwarded write is logged, per scope. C logs the trapped
            // *attempt* before the actual write (gateVc.cc:236-263), so the
            // upstream result is recorded only as the `AllWrites` outcome
            // token, never as a gate on whether to log.
            emit_putlog(
                &env,
                &ctx,
                &pv_name,
                &value_str,
                &old_str,
                WriteAudit::Forwarded {
                    trap: permit.trap,
                    ok: result.is_ok(),
                },
            )
            .await;
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

/// What happened to a client write, for the put-audit decision. The
/// rejection branches report [`Self::Denied`]; the forward path reports
/// [`Self::Forwarded`] with the matched rule's TRAPWRITE mask and the
/// upstream outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteAudit {
    /// The gateway rejected the put before forwarding (read-only mode,
    /// host deny, missing identity, or ACF deny).
    Denied,
    /// The gateway forwarded the put upstream. `trap` is the matched WRITE
    /// rule's `TRAPWRITE` mask; `ok` is whether the upstream write
    /// succeeded.
    Forwarded { trap: bool, ok: bool },
}

/// What a put event logs, as decided by scope + audit outcome — the shape
/// of the line without its value/old payload (which [`emit_putlog`]
/// supplies for the audit variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutLogDecision {
    /// C default trapped-write line: `timestamp user@host pv` only.
    TrapWrite,
    /// Opt-in fail-loud audit line with this outcome token.
    AllWrites(PutOutcome),
}

/// The single owner of the "what does the put log record" contract.
/// Returns `None` to suppress the line, or the [`PutLogDecision`] line
/// shape: [`PutLogDecision::TrapWrite`] is the C-default valueless line
/// (`gateVc.cc:240`); [`PutLogDecision::AllWrites`] is the outcome-tagged
/// audit line.
///
/// C ca-gateway gates *all* put-log emission on the matched rule's
/// `trapMask` and only reaches the write path for access-granted puts
/// (`gateVc.cc:236`): so in `TrapWrite` scope a denied or non-trapped
/// write logs nothing, and a trapped grant logs regardless of the upstream
/// result (C logs the attempt before writing). `AllWrites` is the broader
/// fail-loud superset: every event logs, tagged with its outcome.
fn putlog_outcome(scope: PutLogScope, audit: WriteAudit) -> Option<PutLogDecision> {
    match (scope, audit) {
        // Broader audit: log every event with its outcome token.
        (PutLogScope::AllWrites, WriteAudit::Denied) => {
            Some(PutLogDecision::AllWrites(PutOutcome::Denied))
        }
        (PutLogScope::AllWrites, WriteAudit::Forwarded { ok, .. }) => {
            Some(PutLogDecision::AllWrites(if ok {
                PutOutcome::Ok
            } else {
                PutOutcome::Failed
            }))
        }
        // C contract: denials and non-trapped writes are never logged; a
        // trapped grant logs as the valueless C default line.
        (PutLogScope::TrapWrite, WriteAudit::Denied) => None,
        (PutLogScope::TrapWrite, WriteAudit::Forwarded { trap: true, .. }) => {
            Some(PutLogDecision::TrapWrite)
        }
        (PutLogScope::TrapWrite, WriteAudit::Forwarded { trap: false, .. }) => None,
    }
}

/// Emit (or suppress) the put-audit line for one write event via the
/// single [`putlog_outcome`] owner. `old` is the prior cached value the
/// caller read up-front (see [`cached_old_for_audit`]). Errors from the
/// log write itself are surfaced via `tracing` so a disk-full putlog
/// doesn't silently disappear the audit trail.
async fn emit_putlog(
    env: &WriteHookEnv,
    ctx: &WriteContext,
    pv: &str,
    value: &str,
    old: &str,
    audit: WriteAudit,
) {
    let Some(decision) = putlog_outcome(env.putlog_scope, audit) else {
        return;
    };
    let line = match decision {
        // C default line: value/old are deliberately dropped here, not
        // just omitted from the format — they are never logged by the
        // default `--putlog` build (gateVc.cc:240).
        PutLogDecision::TrapWrite => PutLogLine::TrapWrite,
        PutLogDecision::AllWrites(outcome) => PutLogLine::AllWrites {
            value,
            old,
            outcome,
        },
    };
    if let Some(pl) = &env.putlog
        && let Err(e) = pl.log(&ctx.user, &ctx.host, pv, line).await
    {
        tracing::warn!(
            target: "ca_gateway::putlog",
            error = %e,
            "ca-gateway-rs: putlog write failed"
        );
    }
}

/// Emit a put-audit line for a rejected write. Thin wrapper over
/// [`emit_putlog`] with [`WriteAudit::Denied`] so the four rejection
/// branches stay uniform; the scope gate (denials log only in
/// `AllWrites`) lives in [`putlog_outcome`].
async fn log_denial(env: &WriteHookEnv, ctx: &WriteContext, pv: &str, value: &str, old: &str) {
    emit_putlog(env, ctx, pv, value, old, WriteAudit::Denied).await;
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
    // Only the eight gated tests below host a real async CA server, so the
    // import carries their predicate — otherwise it is unused feature-ON.
    #[cfg(not(feature = "rtems-exec-model"))]
    use epics_ca_rs::server::CaServer;
    // Same predicate as the import above: every `#[serial(epics_env)]` test in
    // this module is one of the gated ones.
    #[cfg(not(feature = "rtems-exec-model"))]
    use serial_test::serial;

    #[test]
    fn native_placeholder_preserves_upstream_type_and_count() {
        // The GET-timeout fallback must advertise the upstream native DBF
        // type + element count, never DBF_DOUBLE/1. CREATE_CHANNEL derives
        // the advertised type/count from value.dbr_type()/value.count(),
        // so pin both for every native CA field type, scalar and array.
        for dbf in [
            DbFieldType::String,
            DbFieldType::Short,
            DbFieldType::Float,
            DbFieldType::Enum,
            DbFieldType::Char,
            DbFieldType::Long,
            DbFieldType::Double,
        ] {
            let scalar = native_placeholder(dbf, 1);
            assert_eq!(scalar.dbr_type(), dbf, "scalar dbr_type for {dbf:?}");
            assert_eq!(scalar.count(), 1, "scalar count for {dbf:?}");

            let array = native_placeholder(dbf, 100);
            assert_eq!(array.dbr_type(), dbf, "array dbr_type for {dbf:?}");
            assert_eq!(array.count(), 100, "array count for {dbf:?}");
        }
        // A zero/absent native count collapses to a 1-element scalar — an
        // empty array would be rejected downstream as a LINK_ALARM.
        assert_eq!(native_placeholder(DbFieldType::Long, 0).count(), 1);
    }

    #[test]
    fn putlog_outcome_trapwrite_scope_matches_c_contract() {
        // C contract (gateVc.cc:236): in TrapWrite scope the put log is
        // gated on the matched rule's trapMask and only the access-granted
        // write path is reached. Enumerate every boundary.
        use PutLogScope::TrapWrite;

        // A granted write whose rule carried TRAPWRITE → one token-less
        // C-compatible line, regardless of the upstream outcome (C logs the
        // attempt before writing).
        assert_eq!(
            putlog_outcome(
                TrapWrite,
                WriteAudit::Forwarded {
                    trap: true,
                    ok: true
                }
            ),
            Some(PutLogDecision::TrapWrite),
            "TRAPWRITE grant must log a C-style (valueless, token-less) line"
        );
        assert_eq!(
            putlog_outcome(
                TrapWrite,
                WriteAudit::Forwarded {
                    trap: true,
                    ok: false
                }
            ),
            Some(PutLogDecision::TrapWrite),
            "TRAPWRITE grant logs the attempt even if the upstream write failed"
        );

        // A granted write to a non-TRAPWRITE rule → no line.
        assert_eq!(
            putlog_outcome(
                TrapWrite,
                WriteAudit::Forwarded {
                    trap: false,
                    ok: true
                }
            ),
            None,
            "non-TRAPWRITE grant must not produce C-style output"
        );

        // A denied write → no line (C never reaches gateVcChan::write).
        assert_eq!(
            putlog_outcome(TrapWrite, WriteAudit::Denied),
            None,
            "denied write must not produce a C-style record"
        );
    }

    #[test]
    fn putlog_outcome_allwrites_scope_logs_every_event_with_token() {
        // AllWrites is the broader fail-loud audit: every event logs with
        // its outcome token, independent of the trap mask.
        use PutLogScope::AllWrites;
        assert_eq!(
            putlog_outcome(AllWrites, WriteAudit::Denied),
            Some(PutLogDecision::AllWrites(PutOutcome::Denied))
        );
        assert_eq!(
            putlog_outcome(
                AllWrites,
                WriteAudit::Forwarded {
                    trap: false,
                    ok: true
                }
            ),
            Some(PutLogDecision::AllWrites(PutOutcome::Ok)),
            "AllWrites logs a non-trapped success as OK"
        );
        assert_eq!(
            putlog_outcome(
                AllWrites,
                WriteAudit::Forwarded {
                    trap: true,
                    ok: false
                }
            ),
            Some(PutLogDecision::AllWrites(PutOutcome::Failed)),
            "AllWrites logs an upstream failure as FAILED"
        );
    }

    fn dummy_env() -> WriteHookEnv {
        WriteHookEnv {
            read_only: false,
            access: Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all())),
            pvlist: Arc::new(ArcSwap::from_pointee(PvList::new())),
            putlog: None,
            putlog_scope: PutLogScope::default(),
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

    /// A held, silent UDP socket that OWNS a dead CA port for the test's
    /// whole lifetime — for the dead-upstream tests.
    ///
    /// Ownership rule: a port is TAKEN by binding it, never probed and
    /// handed on. The old `TcpListener::bind(:0)` + drop probe reserved
    /// nothing after it returned — and never touched the UDP namespace at
    /// all, so a parallel test's `CaServer` (which binds UDP `:0`) could
    /// land on the very same number and answer the `EPICS_CA_SERVER_PORT`
    /// searches, flaking the "dead upstream" false-alive. Binding UDP
    /// `127.0.0.1:0` and keeping the socket (never reading it) instead
    /// guarantees (a) no other socket in the process can take the number,
    /// and (b) every search sent there lands in this socket's buffer and is
    /// never answered — the upstream is deterministically dead. UDP-only
    /// suffices: the CA client only opens a TCP circuit after a UDP search
    /// *reply* names a server, which never comes.
    ///
    /// The caller must bind the returned guard for the test duration — the
    /// port is dead only while the socket lives.
    #[cfg(not(feature = "rtems-exec-model"))]
    fn dead_upstream() -> (std::net::UdpSocket, u16) {
        let sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("own a dead CA port");
        let port = sock.local_addr().unwrap().port();
        (sock, port)
    }

    /// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port` so the
    /// `UpstreamManager`'s internal env-driven `CaClient::new()` connects
    /// to the test server. Callers must be `#[serial(epics_env)]`.
    #[cfg(not(feature = "rtems-exec-model"))]
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
    #[cfg(not(feature = "rtems-exec-model"))]
    async fn pinned_manager(db: Arc<PvDatabase>) -> UpstreamManager {
        pinned_manager_full(db, Arc::new(RwLock::new(PvCache::new())), CacheMode::Cached).await
    }

    /// Like [`pinned_manager`] but with a caller-supplied cache (so a test
    /// can inspect per-PV state) and an explicit cache mode (so a test can
    /// exercise the no-cache GET-forward + lazy-monitor paths).
    #[cfg(not(feature = "rtems-exec-model"))]
    async fn pinned_manager_full(
        db: Arc<PvDatabase>,
        cache: Arc<RwLock<PvCache>>,
        cache_mode: CacheMode,
    ) -> UpstreamManager {
        let env = dummy_env();
        UpstreamManager::new(UpstreamManagerConfig {
            cache,
            shadow_db: db,
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            putlog_scope: PutLogScope::default(),
            stats: env.stats.clone(),
            read_only: false,
            cache_mode,
            connect_timeout: Duration::from_secs(1),
            event_mask: crate::ca_gateway::server::DEFAULT_EVENT_MASK,
            beacon_anomaly: env.beacon_anomaly.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        })
        .await
        .expect("manager builds")
    }

    // `UpstreamManager::new` constructs the gateway's upstream `CaClient`.
    // Under this feature that client's search engine is name-servers-only
    // (`epics-ca-rs` `search::SearchTransport` has no `Udp` variant on the
    // exec backend, because a future spawned through the `runtime::task`
    // seam runs on a callback-pool worker with no tokio reactor), and a
    // name-servers-only engine with an empty `EPICS_CA_NAME_SERVERS` is
    // refused at construction — it could reach no server at all. The
    // gateway is a hosted daemon that is never built in the exec model, so
    // the configuration these tests use is not one it has to satisfy.
    #[cfg(not(feature = "rtems-exec-model"))]
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
            putlog_scope: PutLogScope::default(),
            stats: env.stats.clone(),
            read_only: false,
            cache_mode: CacheMode::Cached,
            connect_timeout: Duration::from_secs(1),
            event_mask: crate::ca_gateway::server::DEFAULT_EVENT_MASK,
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
    // Same reason as `manager_construct`: the manager builds a
    // name-servers-only upstream `CaClient` with no name server under this
    // feature.
    #[cfg(not(feature = "rtems-exec-model"))]
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
            putlog_scope: PutLogScope::default(),
            stats: env.stats.clone(),
            read_only: false,
            cache_mode: CacheMode::Cached,
            connect_timeout: Duration::from_secs(1),
            event_mask: crate::ca_gateway::server::DEFAULT_EVENT_MASK,
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
        let upstream_read = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl {
            asg: Some("DEFAULT".to_string()),
            asl: 0,
        }));
        let hook = build_access_hook(acl, access, upstream_write, upstream_read);

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
        let upstream_read = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl { asg: None, asl: 0 }));
        let hook = build_access_hook(acl, access, upstream_write, upstream_read);
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
        let upstream_read = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl { asg: None, asl: 0 }));
        let hook = build_access_hook(acl, access, upstream_write.clone(), upstream_read);

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

    /// GW-40: read-access must bridge the upstream IOC's `ca_read_access`
    /// symmetrically with write — `local_acf_read && upstream_read` — so an
    /// upstream that revokes read to the gateway's client reports read=false
    /// downstream even under a local allow-all ACF. Mirrors C
    /// `gateVcChan::readAccess` (gateVc.cc:326):
    /// `asclient->readAccess() && vc->readAccess()`.
    #[test]
    fn br_gw40_upstream_read_denied_overrides_local_acf_allow() {
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let upstream_write = Arc::new(AtomicBool::new(true));
        let upstream_read = Arc::new(AtomicBool::new(false));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl { asg: None, asl: 0 }));
        let hook = build_access_hook(acl, access, upstream_write, upstream_read.clone());

        let d = hook("alice", "host1");
        assert!(
            !d.read,
            "upstream read-denied must override local allow-all"
        );
        assert!(d.write, "write must still be granted by allow-all");

        // Restoring upstream read-access restores read permission.
        upstream_read.store(true, Ordering::Relaxed);
        let d2 = hook("alice", "host1");
        assert!(d2.read, "read must be granted once upstream restores it");
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
        let upstream_read = Arc::new(AtomicBool::new(true));
        let acl = Arc::new(ArcSwap::from_pointee(PvAcl {
            asg: Some("OldGroup".to_string()),
            asl: 1,
        }));
        let hook = build_access_hook(acl.clone(), access, upstream_write, upstream_read);

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
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_2026_111_reload_updates_live_acl_on_admitted_pv() {
        let name = "AS:reload:pv";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

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

    /// GW-23: when the connect-time DBR_CTRL metadata seed does NOT land,
    /// the property monitor's first event must SEED metadata rather than
    /// skip it as a redundant confirmation — otherwise a stable PV's
    /// units/precision/limits stay zeroed until a second property change
    /// that may never come. C avoids the gap by enabling `propMonitor()`
    /// only after `getCB` seeds (gatePv.cc:1702-1705); we instead make the
    /// first-event skip conditional on the seed outcome (`seed_succeeded`).
    /// Forcing a zero seed budget makes every connect-time seed miss, so a
    /// non-empty downstream `units` proves the first-event recovery seeded
    /// it. Reverting `first_event_seen = !seed_succeeded` to `= false`
    /// leaves units empty and fails this test.
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_gw23_seed_miss_first_prop_event_seeds_metadata() {
        use epics_base_rs::server::snapshot::{DisplayInfo, Snapshot};

        let name = "GW23:seed:pv";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();

        // Grab the upstream db before `server` moves into the run task, so
        // we can give the UPSTREAM PV real display metadata (units="mm").
        // A seeded shadow is then distinguishable from an unseeded one — the
        // negotiation GET seeds only value, never display (upstream.rs:577).
        let up_db = server.database().clone();
        let _server = tokio::spawn(async move { server.run().await });

        let mut ctrl = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        ctrl.display = Some(DisplayInfo {
            units: "mm".into(),
            precision: 3,
            ..Default::default()
        });
        up_db
            .set_pv_metadata(name, &ctrl)
            .await
            .expect("upstream PV display metadata installed");

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mut mgr = pinned_manager(db.clone()).await;
        // Force every connect-time CTRL seed to miss (private test seam) so
        // the recovery path — first property event seeds — is exercised.
        mgr.metadata_seed_timeout = Duration::ZERO;

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        // Precondition: the seed genuinely MISSED — the shadow's units must
        // be empty right after ensure_subscribed, before the prop task runs.
        // Without this guard the test would pass vacuously if the seed ever
        // silently succeeded (the units would already be present).
        {
            let pv = db.find_pv(name).await.expect("shadow registered");
            let immediate = pv
                .snapshot()
                .display
                .map(|d| d.units.to_string())
                .unwrap_or_default();
            assert_eq!(
                immediate, "",
                "seed must have missed (ZERO budget) leaving units empty, so \
                 the first-event recovery is what this test exercises"
            );
        }

        // The cached property task subscribes DBE_PROPERTY, receives its
        // initial-state event, and — because the seed missed — consumes it
        // to seed metadata (get_with_metadata → post_pv_property). Poll the
        // shadow until its display units appear (bounded ~2 s).
        let mut units = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(pv) = db.find_pv(name).await {
                if let Some(d) = pv.snapshot().display {
                    let u = d.units.to_string();
                    if !u.is_empty() {
                        units = u;
                        break;
                    }
                }
            }
        }
        assert_eq!(
            units, "mm",
            "seed missed → the property monitor's first event must seed the \
             shadow metadata (GW-23), not leave units zeroed"
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
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_fr2_alias_registers_shadow_pv_under_served_name() {
        // Served (alias) name differs from the resolved real PV.
        let served = "Beam:current";
        let real = "SR:DCCT:current";

        let server = CaServer::builder()
            .port(0)
            .pv(real, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

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
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_fr2_non_alias_keys_shadow_pv_by_same_name() {
        let name = "Plain:pv";

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(2.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

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
    // Same reason as `manager_construct`: the manager builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_r64_dead_upstream_not_registered() {
        // A held dead port with NO server bound: the upstream search never
        // resolves and the configured connect_timeout expires. The guard
        // stays alive for the whole test so no parallel `CaServer` can land
        // on the number and answer.
        let (_dead, port) = dead_upstream();
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

    /// No-cache mode: a downstream GET must serve a FRESH upstream fetch,
    /// not the stored shadow snapshot. After subscribe, overwrite the
    /// shadow's stored value with a sentinel; the read hook (no-cache only)
    /// must return the upstream value, while `snapshot` still returns the
    /// sentinel — proving the GET path forwards and the monitor path does
    /// not. Mirrors C `-no_cache` forwarding the read to the IOC
    /// (gateVc.cc:1361-1369).
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br24_no_cache_get_forwards_to_upstream() {
        let name = "NC:get";
        const UPSTREAM: f64 = 3.0;
        const SENTINEL: f64 = 999.0;

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(UPSTREAM))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager_full(
            db.clone(),
            Arc::new(RwLock::new(PvCache::new())),
            CacheMode::NoCache,
        )
        .await;

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        let pv = db.find_pv(name).await.expect("shadow PV registered");
        // Make the stored shadow value a sentinel the fresh fetch must
        // override — proving the GET goes upstream, not to the cache.
        pv.set(EpicsValue::Double(SENTINEL));

        let read = pv.read_snapshot().await.expect("no-cache GET forwards");
        assert_eq!(
            read.value,
            EpicsValue::Double(UPSTREAM),
            "no-cache GET must serve the fresh upstream value, not the sentinel"
        );
        // The monitor/stored path still serves the sentinel — the read
        // hook is GET-path only.
        assert_eq!(
            pv.snapshot().value,
            EpicsValue::Double(SENTINEL),
            "snapshot (monitor path) must still serve the stored shadow value"
        );

        mgr.shutdown().await;
    }

    /// Cached mode regression: NO read hook is installed, so a GET serves
    /// the stored shadow value even when the upstream differs — the
    /// no-cache forwarding is opt-in and does not leak into cached mode.
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br24_cached_get_serves_shadow_value() {
        let name = "C:get";
        const SENTINEL: f64 = 999.0;

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(3.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await; // Cached

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        let pv = db.find_pv(name).await.expect("shadow PV registered");

        // The cached-mode subscribe above leaves the upstream monitor's first
        // event in flight, and the forwarding task writes it into the very
        // cell this test is about to plant SENTINEL in. Land that write first,
        // or the two writers race and the event can overwrite the sentinel in
        // the window between the set and the read below. `post_event_count` is
        // bumped immediately after `put_pv_and_post_snapshot` returns Ok and is
        // monotonic, so reading it is correct whichever side wins; only this
        // one PV is subscribed, so the count is unambiguous. Upstream never
        // changes value again and the property task writes metadata only, so
        // no later event can reach the value cell.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while mgr.write_env.stats.post_event_count.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the upstream monitor's first event never reached the shadow PV; \
                 without it this test cannot tell a cached GET from a lost race"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        pv.set(EpicsValue::Double(SENTINEL));

        let read = pv.read_snapshot().await.expect("cached GET never errors");
        assert_eq!(
            read.value,
            EpicsValue::Double(SENTINEL),
            "cached GET must serve the stored shadow value (no read hook)"
        );

        mgr.shutdown().await;
    }

    /// No-cache mode: the upstream monitor is lazy. After `ensure_subscribed`
    /// no forwarding task runs; `ensure_monitor` (first downstream monitor)
    /// spawns one; `release_monitor` (last downstream monitor) aborts it.
    /// Mirrors C no-cache `getCB` creating the monitor only on
    /// `needPosting()` (gatePv.cc:1737-1753).
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br24_no_cache_monitor_is_lazy() {
        let name = "NC:mon";

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager_full(
            db.clone(),
            Arc::new(RwLock::new(PvCache::new())),
            CacheMode::NoCache,
        )
        .await;

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects");

        // No monitor task until a downstream client subscribes.
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_none(),
            "no-cache: no forwarding task before the first downstream monitor"
        );

        // First downstream monitor → task spawned.
        mgr.ensure_monitor(name);
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_some(),
            "ensure_monitor must spawn the forwarding task"
        );

        // A second monitor-open is idempotent (task already present).
        mgr.ensure_monitor(name);
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_some(),
            "second ensure_monitor is a no-op while a task runs"
        );

        // Last downstream monitor → task aborted.
        mgr.release_monitor(name);
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_none(),
            "release_monitor must abort the forwarding task"
        );

        mgr.shutdown().await;
    }

    /// Cached mode regression: the monitor is eager (spawned at
    /// `ensure_subscribed`) and `ensure_monitor`/`release_monitor` are
    /// no-ops — the persistent monitor must outlive any single downstream
    /// subscriber.
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br24_cached_monitor_eager_ensure_release_noop() {
        let name = "C:mon";

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await; // Cached

        mgr.ensure_subscribed(name, name, None, 0)
            .await
            .expect("ensure_subscribed connects");

        // Cached: forwarding task exists immediately.
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_some(),
            "cached: forwarding task is spawned eagerly at ensure_subscribed"
        );

        // ensure_monitor / release_monitor are no-ops in cached mode.
        mgr.ensure_monitor(name);
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_some(),
            "cached: ensure_monitor is a no-op (task stays)"
        );
        mgr.release_monitor(name);
        assert!(
            mgr.subs
                .lock()
                .get(name)
                .expect("sub tracked")
                .task
                .is_some(),
            "cached: release_monitor must NOT abort the persistent monitor"
        );

        mgr.shutdown().await;
    }

    /// the lazy-resolution connect gate must honor the configured
    /// `CacheTimeouts::connect_timeout`, not a hard-coded constant. Build
    /// a manager with a short 150 ms budget against a dead port and assert
    /// the search miss is reported well under the old 1 s constant —
    /// proving the configured value flows into `wait_connected` (one
    /// connect-timeout owner shared with the cache reaper).
    // Same reason as `manager_construct`: the manager builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br_2026_22_lazy_connect_honors_configured_timeout() {
        // Deliberately a dead port: nothing is served here, so the
        // configured connect timeout is what ends the search. Hold the
        // guard for the whole test so a parallel `CaServer` cannot land on
        // the number and resolve the search early.
        let (_dead, port) = dead_upstream();
        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let env = dummy_env();
        let mgr = UpstreamManager::new(UpstreamManagerConfig {
            cache: Arc::new(RwLock::new(PvCache::new())),
            shadow_db: db.clone(),
            access: env.access.clone(),
            pvlist: env.pvlist.clone(),
            putlog: None,
            putlog_scope: PutLogScope::default(),
            stats: env.stats.clone(),
            read_only: false,
            cache_mode: CacheMode::Cached,
            connect_timeout: Duration::from_millis(150),
            event_mask: crate::ca_gateway::server::DEFAULT_EVENT_MASK,
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
