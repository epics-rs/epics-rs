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

// RTEMS-EXEC-MODEL-ALLOW(2): checked - `cached_old_for_audit_boundaries` and
// `connect_rights_resyncs_a_lagged_receiver` run and pass in the exec-backend
// suite (the cache, and a bare broadcast channel; never the upstream client).
// The others take gate (3); see the comment on `manager_construct`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use epics_base_rs::error::{CaError, CaOp, CaResult, DbStatus};
use epics_base_rs::runtime::sync::broadcast;
use epics_base_rs::runtime::task::TaskHandle;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{PvWrite, WriteContext, WriteDone, WriteForward, WriteHook};
use epics_base_rs::server::record::AlarmAck;
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::{
    DBR_PUT_ACKS, DBR_PUT_ACKT, DbFieldType, EpicsValue, PvString, RecordName,
};
use epics_ca_rs::client::{AccessRights, CaChannel, CaClient, ConnectionEvent, MonitorHandle};
use epics_ca_rs::protocol::{DBE_PROPERTY, ECA_DISCONN, ECA_DISCONNCHID};
#[cfg(tokio_backend)]
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
        DbFieldType::String => EpicsValue::StringArray(vec![PvString::new(); n].into()),
        DbFieldType::Short if scalar => EpicsValue::Short(0),
        DbFieldType::Short => EpicsValue::ShortArray(vec![0; n].into()),
        DbFieldType::Float if scalar => EpicsValue::Float(0.0),
        DbFieldType::Float => EpicsValue::FloatArray(vec![0.0; n].into()),
        DbFieldType::Enum if scalar => EpicsValue::Enum(0),
        DbFieldType::Enum => EpicsValue::EnumArray(vec![0; n].into()),
        DbFieldType::Char if scalar => EpicsValue::Char(0),
        DbFieldType::Char => EpicsValue::CharArray(vec![0; n].into()),
        DbFieldType::Long if scalar => EpicsValue::Long(0),
        DbFieldType::Long => EpicsValue::LongArray(vec![0; n].into()),
        DbFieldType::Double if scalar => EpicsValue::Double(0.0),
        DbFieldType::Double => EpicsValue::DoubleArray(vec![0.0; n].into()),
        // CA wire types 7/8 do not exist: a CA upstream channel never
        // reports Int64/UInt64 natively (they are internal record types
        // that travel over CA as DBR_DOUBLE). Mirror that DBR mapping —
        // their `dbr_type()` is `Double`, so the advertised create-channel
        // type stays DBF_DOUBLE even on this unreachable branch.
        DbFieldType::Int64 if scalar => EpicsValue::Int64(0),
        DbFieldType::Int64 => EpicsValue::Int64Array(vec![0; n].into()),
        DbFieldType::UInt64 if scalar => EpicsValue::UInt64(0),
        DbFieldType::UInt64 => EpicsValue::UInt64Array(vec![0; n].into()),
        // Like Int64/UInt64, DBF_USHORT/DBF_ULONG never travel natively over
        // CA (they promote to DBR_LONG / DBR_DOUBLE), so these branches are
        // unreachable for a CA upstream; mapped for completeness.
        DbFieldType::UShort if scalar => EpicsValue::UShort(0),
        DbFieldType::UShort => EpicsValue::UShortArray(vec![0; n].into()),
        DbFieldType::ULong if scalar => EpicsValue::ULong(0),
        DbFieldType::ULong => EpicsValue::ULongArray(vec![0; n].into()),
        // DBF_UCHAR promotes to DBR_CHAR over CA (db_convert.h), so a CA
        // upstream reports it as DBF_CHAR, not DBF_UCHAR — this branch is
        // unreachable for a CA upstream but mapped for completeness. Its
        // `dbr_type()` is `Char`, keeping the advertised create-channel type
        // DBF_CHAR on this branch.
        DbFieldType::UChar if scalar => EpicsValue::UChar(0),
        DbFieldType::UChar => EpicsValue::UCharArray(vec![0; n].into()),
    }
}

/// What a shadow PV holds before upstream sent it a value: the connected
/// channel's native type and count, which CREATE_CHAN advertises.
fn channel_placeholder(channel: &CaChannel) -> EpicsValue {
    match (channel.native_field_type(), channel.element_count()) {
        (Ok(dbf), Ok(n)) => native_placeholder(dbf, n),
        // Pathological: metadata unreadable despite a connected channel.
        // Keep the scalar-double fallback rather than failing outright.
        _ => EpicsValue::Double(0.0),
    }
}

/// The shadow metadata a `DBR_CTRL_*` get carries (C `setPvData`).
fn ctrl_metadata(snap: &Snapshot) -> epics_base_rs::server::pv::PvMetadata {
    epics_base_rs::server::pv::PvMetadata {
        display: snap.display.clone(),
        control: snap.control.clone(),
        enums: snap.enums.clone(),
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
/// the current value, so a reload that calls `UpstreamManager::update_acl`
/// is observed by both hooks without rebuilding them.
type PvAclCell = Arc<ArcSwap<PvAcl>>;

/// One upstream subscription: the long-lived [`CaChannel`] is shared
/// between the monitor-forwarding task and any direct
/// `UpstreamManager::put` / `UpstreamManager::get` calls so we
/// don't pay a fresh CREATE_CHAN round-trip per write. Mirrors C
/// CA-gateway behaviour (cas/io/casChannel.cc) where the upstream
/// channel is reused across the gateway's lifetime.
struct UpstreamSubscription {
    channel: Arc<CaChannel>,
    /// The monitor-forwarding task. `Some` whenever an upstream
    /// subscription is live: always in [`CacheMode::Cached`], and in
    /// [`CacheMode::NoCache`] only while ≥1 downstream client is
    /// monitoring (created by `UpstreamManager::ensure_monitor`, torn
    /// down by `UpstreamManager::release_monitor`). `None` for a
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
    /// `UpstreamManager::ensure_prop_monitor`, torn down by
    /// `UpstreamManager::release_prop_monitor`). `None` otherwise.
    /// Mirrors C ca-gateway's separate `pv->propMonitor()`
    /// (`gatePv.cc:1705`, `:1749-1752`).
    prop_task: Option<TaskHandle<()>>,
    /// Whether the connect-time DBR_CTRL metadata seed actually landed.
    /// When `false` (the 500 ms seed timed out or errored), a
    /// [`Self::prop_task`] spawned later (the no-cache lazy path in
    /// `UpstreamManager::ensure_prop_monitor`) must NOT skip its first
    /// DBE_PROPERTY event — that event carries the full metadata and seeds
    /// what the connect-time get missed. The cached path reads this from the
    /// local seed outcome directly; storing it here lets the lazy path honor
    /// the same invariant. See `UpstreamManager::spawn_prop_forward_task`.
    seed_succeeded: bool,
    /// Live `.pvlist` ASG/ASL for this shadow PV, shared by-`Arc` with
    /// the read and write hook closures so a `AS`/`PVL` reload can swap
    /// the group/level in place. Replaces the previous by-value
    /// `asg`/`asl` capture that left hooks enforcing the identity
    /// resolved at first subscription forever.
    acl: PvAclCell,
    /// The `ChannelEventOwner` task. Aborted on drop (when the
    /// subscription is removed via `unsubscribe`).
    channel_events: AbortOnDrop,
    /// The shadow PV's life, which the forwarding tasks follow.
    life: tokio::sync::watch::Receiver<ShadowLife>,
    /// Where a forwarding task hands a [`ShadowLife::Pending`] life's event
    /// to the `ChannelEventOwner`.
    first_events: FirstEventTx,
}

/// An event of a [`ShadowLife::Pending`] life, offered to the owner to
/// ready the PV with (`offer_first_event`).
struct FirstEvent {
    life: u64,
    snapshot: Snapshot,
    /// From the property monitor: a control get, whose metadata the PV takes
    /// before the value (`setPvData`, gatePv.cc:1580).
    property: bool,
    /// The event back when another event readied the life first, for the
    /// sender to post.
    returned: epics_base_rs::runtime::sync::oneshot::Sender<Option<Snapshot>>,
}

type FirstEventTx = tokio::sync::mpsc::UnboundedSender<FirstEvent>;

/// Offer an event of life `lived` to the owner while that life is pending:
/// `None` once it readied the PV with it or the life ended, else the event
/// back to post.
#[cfg(tokio_backend)]
async fn offer_first_event(
    first_events: &FirstEventTx,
    life: &mut tokio::sync::watch::Receiver<ShadowLife>,
    lived: Option<u64>,
    snapshot: Snapshot,
    property: bool,
) -> Option<Snapshot> {
    let Some(n) = lived.filter(|&n| *life.borrow() == ShadowLife::Pending(n)) else {
        return Some(snapshot);
    };
    let (returned, back) = epics_base_rs::runtime::sync::oneshot::channel();
    first_events
        .send(FirstEvent {
            life: n,
            snapshot,
            property,
            returned,
        })
        .ok()?;
    tokio::select! {
        back = back => back.ok().flatten(),
        // The follower drops a dead life's monitor without the owner.
        _ = life.wait_for(|l| l.armed() != Some(n)) => None,
    }
}

/// One registration of the shadow PV: C `gatePvData` between `life()` and
/// `death()`. The number counts the lives, so a follower can tell a revival
/// from the life it already served.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShadowLife {
    Alive(u64),
    /// Cached mode: the PV is registered [`pending`] and the monitors are
    /// armed; their first event readies it, as C `getCB` arms both
    /// (gatePv.cc:1703-1705) and `eventCB` / `propEventCB` `vcAdd` the vc
    /// (:1432-1441, 1598-1602).
    ///
    /// [`pending`]: epics_base_rs::server::pv::ProcessVariable::pending
    Pending(u64),
    /// No PV registered and no monitor armed.
    Dead(u64),
}

impl ShadowLife {
    /// The life the monitors serve, a pending one included.
    fn armed(self) -> Option<u64> {
        match self {
            Self::Alive(n) | Self::Pending(n) => Some(n),
            Self::Dead(_) => None,
        }
    }

    /// Upstream is gone and the shadow PV removed.
    fn die(&mut self) {
        if let Self::Alive(n) | Self::Pending(n) = *self {
            *self = Self::Dead(n);
        }
    }

    /// Cached mode, the next life's PV registered: its monitors are armed.
    fn arm(&mut self) {
        if let Self::Dead(n) = *self {
            *self = Self::Pending(n + 1);
        }
    }

    /// No-cache, the next life's PV registered; or cached, the pending life
    /// readied.
    fn revive(&mut self) {
        match *self {
            Self::Dead(n) => *self = Self::Alive(n + 1),
            Self::Pending(n) => *self = Self::Alive(n),
            Self::Alive(_) => {}
        }
    }
}

/// Aborts the channel-event owner with the subscription that owns it.
struct AbortOnDrop(TaskHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `ensure_subscribed` hands its channel to `reached` and waits on `release`
/// before it starts the access-rights watcher.
#[cfg(test)]
struct WatchGate {
    reached: epics_base_rs::runtime::sync::oneshot::Sender<Arc<CaChannel>>,
    release: epics_base_rs::runtime::sync::oneshot::Receiver<()>,
}

/// The channel's current rights, for a receiver that lagged: the events it
/// lost may be the ones that said so. `None` while disconnected.
async fn current_rights(channel: &CaChannel) -> Option<AccessRights> {
    channel.info().await.ok().map(|i| i.access_rights)
}

/// The rights the client sends just before `Connected` (C order,
/// `camessage.c:1180/1201`; `nciu.cpp:170-177` clears them at a
/// disconnect), from a receiver taken with the channel. A `Connected` with
/// none since the last `Disconnected`, which only a lag leaves, asks
/// `resync` ([`current_rights`]), as a lag does. `None` once the client is
/// gone.
async fn connect_rights<F>(
    rx: &mut broadcast::Receiver<ConnectionEvent>,
    resync: impl Fn() -> F,
) -> Option<AccessRights>
where
    F: std::future::Future<Output = Option<AccessRights>>,
{
    // Rights since the last `Disconnected`, which a following `Connected` owns.
    let mut held = None;
    loop {
        let lost = match rx.recv().await {
            Ok(ConnectionEvent::Connected) => match held {
                Some(rights) => return Some(rights),
                None => true,
            },
            Ok(ConnectionEvent::AccessRightsChanged { read, write }) => {
                held = Some(AccessRights { read, write });
                false
            }
            Ok(ConnectionEvent::Disconnected) => {
                held = None;
                false
            }
            Ok(_) => false,
            // What is left in the receiver may be a past connect's pair.
            Err(broadcast::error::RecvError::Lagged(_)) => true,
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        if lost {
            match resync().await {
                Some(rights) => return Some(rights),
                None => held = None,
            }
        }
    }
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

/// Configuration handed to `UpstreamManager::new`. Groups the
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
    /// upstream monitor until `UpstreamManager::ensure_monitor` is
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

/// C `gatePvData::connectCB` and `accessCB` for one subscription: the one
/// consumer of the channel's connection events, and the only writer of the
/// shadow PV's [`ShadowLife`]. Death and revival happen here and nowhere
/// else, whatever upstream monitors are armed.
#[cfg(tokio_backend)]
struct ChannelEventOwner {
    channel: std::sync::Weak<CaChannel>,
    name: RecordName,
    cache: Arc<RwLock<PvCache>>,
    db: Arc<PvDatabase>,
    life: tokio::sync::watch::Sender<ShadowLife>,
    /// The shadow PV the last death removed, kept for its hooks; `Some`
    /// while dead.
    retired: Option<Arc<epics_base_rs::server::pv::ProcessVariable>>,
    notifier: Arc<ArcSwapOption<AccessRightsNotifier>>,
    beacon_anomaly: Arc<super::beacon::BeaconAnomaly>,
    /// Bound on the revival's control get (`metadata_seed_timeout`); zero
    /// skips it.
    ctrl_timeout: Duration,
    shown: AccessRights,
    /// Cached mode: a revival registers pending and its monitors' first
    /// event readies it.
    cached: bool,
}

#[cfg(tokio_backend)]
impl ChannelEventOwner {
    async fn run(
        mut self,
        mut events: broadcast::Receiver<ConnectionEvent>,
        mut first_events: tokio::sync::mpsc::UnboundedReceiver<FirstEvent>,
    ) {
        loop {
            let event = tokio::select! {
                event = events.recv() => event,
                Some(first) = first_events.recv() => {
                    self.first_event(first);
                    continue;
                }
            };
            let Some(channel) = self.channel.upgrade() else {
                break;
            };
            match event {
                Ok(ConnectionEvent::Disconnected) => self.death().await,
                Ok(ConnectionEvent::Connected) => self.revive(&channel).await,
                Ok(ConnectionEvent::AccessRightsChanged { .. }) => self.access(&channel),
                Ok(ConnectionEvent::NativeTypeChanged { .. }) => {}
                // The lost events may have been a death or a revival.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if channel.info().await.is_ok() {
                        self.revive(&channel).await;
                    } else {
                        self.death().await;
                    }
                    self.access(&channel);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    /// `gatePvData::death` (gatePv.cc:575-601): mark the cache entry
    /// disconnected and DELETE the downstream virtual channel.
    ///
    /// `delete vc; vc = NULL;` (`:600-601`) is the whole point — the
    /// downstream monitors STOP. Removing the shadow PV destroys it, which
    /// retires every downstream channel with `CA_PROTO_SERVER_DISCONN` and
    /// drops the GETs parked on a pending one unanswered; while it is gone
    /// the name answers does-not-exist, C's `pverDoesNotExistHere` (`:622`).
    /// The followers drop their upstream monitors on the transition, as
    /// `death` unmonitors (`:658-661`).
    async fn death(&mut self) {
        if let ShadowLife::Dead(_) = *self.life.borrow() {
            return;
        }
        if let Some(entry) = self.cache.read().await.get(&self.name) {
            entry.write().await.set_state(PvState::Disconnect);
        }
        self.retired = self.db.remove_simple_pv(&self.name).await;
        self.life.send_modify(ShadowLife::die);
    }

    /// `gatePvData::life` then `activate` → `get(ctrlType)` → `getCB` →
    /// `setPvData` (gatePv.cc:505-556, 398-401, 1693-1695): a fresh shadow PV
    /// with the dead one's hooks and the control get's metadata. A failed
    /// control get keeps the dead PV's metadata, which the property monitor
    /// then refreshes. Cached, the PV is registered [`pending`] at once, as
    /// `life()` answers the exist tests at connect (`:512-518`), and the get
    /// is followed by `monitor()` + `propMonitor()` (`:1703-1705`), whose
    /// first event readies it ([`Self::first_event`]); no-cache `activate`
    /// adds the vc with the get's value (`:397-398`).
    ///
    /// [`pending`]: epics_base_rs::server::pv::ProcessVariable::pending
    async fn revive(&mut self, channel: &CaChannel) {
        if !matches!(*self.life.borrow(), ShadowLife::Dead(_)) {
            return;
        }
        let Some(dead) = self.retired.clone() else {
            return;
        };
        // `life()`'s state change (gatePv.cc:536), ahead of the get and the
        // monitors.
        if let Some(entry) = self.cache.read().await.get(&self.name) {
            let mut entry = entry.write().await;
            if entry.state == PvState::Disconnect {
                let next = if entry.subscriber_count() > 0 {
                    PvState::Active
                } else {
                    PvState::Inactive
                };
                entry.set_state(next);
            }
        }
        if self.cached {
            let placeholder = channel_placeholder(channel);
            if !self.register(dead.respawn(placeholder).pending()).await {
                return;
            }
            if let Some(snap) = self.ctrl_get(channel).await
                && let Some(pv) = self.db.find_pv(&self.name)
            {
                pv.set_metadata(ctrl_metadata(&snap));
            }
            self.life.send_modify(ShadowLife::arm);
            return;
        }
        let ctrl = self.ctrl_get(channel).await;
        if let Some(snap) = &ctrl {
            dead.set_metadata(ctrl_metadata(snap));
        }
        let value = ctrl.map_or_else(|| dead.snapshot().value, |snap| snap.value);
        if self.register(dead.respawn(value)).await {
            self.life.send_modify(ShadowLife::revive);
        }
    }

    /// The revival's control get, bounded by `ctrl_timeout`.
    async fn ctrl_get(&self, channel: &CaChannel) -> Option<Snapshot> {
        if self.ctrl_timeout.is_zero() {
            return None;
        }
        epics_base_rs::runtime::task::timeout(
            self.ctrl_timeout,
            channel.get_with_metadata(DbrClass::Ctrl),
        )
        .await
        .ok()
        .and_then(Result::ok)
    }

    /// C `eventCB` / `propEventCB` on a pending life (gatePv.cc:1432-1441,
    /// 1580-1602): its first event readies the PV — `vcAdd`, which answers
    /// the reads queued meanwhile and gives each monitor its first event —
    /// and one that raced it in is handed back to post.
    fn first_event(&mut self, event: FirstEvent) {
        let life = *self.life.borrow();
        let back = if life == ShadowLife::Pending(event.life) {
            if let Some(pv) = self.db.find_pv(&self.name) {
                if event.property {
                    pv.set_metadata(ctrl_metadata(&event.snapshot));
                }
                pv.make_ready(event.snapshot);
            }
            self.life.send_modify(ShadowLife::revive);
            None
        } else if life == ShadowLife::Alive(event.life) {
            Some(event.snapshot)
        } else {
            None
        };
        let _ = event.returned.send(back);
    }

    /// Register the revived shadow PV; `false` if it could not be.
    async fn register(&mut self, replacement: epics_base_rs::server::pv::ProcessVariable) -> bool {
        if let Err(e) = self.db.add_simple_pv(replacement).await {
            tracing::debug!(
                pv = %self.name,
                error = %e,
                "ca-gateway-rs: shadow PV re-registration after upstream reconnect failed"
            );
            return false;
        }
        self.retired = None;
        // `life()` for a disconnected PV (gatePv.cc:543-545).
        self.beacon_anomaly.request();
        true
    }

    /// C accessCB (gatePv.cc:1851-1852) updates BOTH read and write from
    /// ca_read_access/ca_write_access; postAccessRights re-pushes when the
    /// computed level changed on either bit. The hook reads the channel
    /// itself, so this only wakes downstream clients when either bit
    /// flipped; a lag loses nothing the hook reads.
    fn access(&mut self, channel: &CaChannel) {
        let now = channel.access_rights();
        if (now.read, now.write) != (self.shown.read, self.shown.write) {
            self.shown = now;
            if let Some(n) = self.notifier.load_full() {
                n.notify();
            }
        }
    }
}

/// Manages upstream CA client connections for the gateway.
///
/// Holds a single shared `CaClient` and tracks per-PV channel +
/// monitor task pairs. The channel is reused for PUT / GET so the
/// gateway does not re-do CA handshake on every write.
#[cfg(tokio_backend)]
pub struct UpstreamManager {
    /// The executor every upstream forwarding task is spawned through.
    /// Taken in `UpstreamManager::new`, which is the gateway's
    /// upstream-owner creation point, so the sync `ensure_*` entry points
    /// do not have to re-derive it from whatever thread calls them.
    reactor: epics_base_rs::runtime::task::Reactor,
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
    subs: parking_lot::Mutex<HashMap<RecordName, UpstreamSubscription>>,
    /// In-flight first-create dedupe. A second concurrent caller for
    /// the same PV awaits the Notify instead of duplicating the
    /// upstream channel + subscribe + spawn work.
    pending: parking_lot::Mutex<HashMap<RecordName, Arc<tokio::sync::Notify>>>,
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
    /// Test-only pause between the rights seed and the watcher's first read,
    /// where a rights event must not be lost.
    #[cfg(test)]
    watch_gate: parking_lot::Mutex<Option<WatchGate>>,
    /// Test-only hold on the next property task before it subscribes, so a
    /// test can read the shadow before the first property event seeds it.
    #[cfg(test)]
    prop_gate: parking_lot::Mutex<Option<epics_base_rs::runtime::sync::oneshot::Receiver<()>>>,
    /// Test-only hold on the next value task before it takes an event.
    #[cfg(test)]
    value_gate: parking_lot::Mutex<Option<epics_base_rs::runtime::sync::oneshot::Receiver<()>>>,
    /// Detachable handle that re-pushes `CA_PROTO_ACCESS_RIGHTS` to
    /// already-connected downstream clients. Installed via
    /// [`Self::install_access_notifier`] once the downstream `CaServer`
    /// exists (the manager is built first), so it is wrapped in an
    /// `ArcSwapOption` for lock-free interior mutability. Fired whenever a
    /// gateway-side change flips a channel's computed downstream access —
    /// an upstream IOC access-rights event (the subscription's watcher) or an
    /// `.acf`/`.pvlist` reload re-resolving per-PV ACLs
    /// ([`Self::notify_downstream_access_change`]). Mirrors C ca-gateway
    /// `postAccessRights` / `gateChan::resetAsClient` (gateVc.cc:170-199,
    /// 1624-1638) and RSRV `asComputeAllAsg`.
    access_notifier: Arc<ArcSwapOption<AccessRightsNotifier>>,
}

#[cfg(tokio_backend)]
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
            reactor: epics_base_rs::runtime::task::Reactor::current()
                .expect("UpstreamManager::new is awaited on the gateway's reactor"),
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
            #[cfg(test)]
            watch_gate: parking_lot::Mutex::new(None),
            #[cfg(test)]
            prop_gate: parking_lot::Mutex::new(None),
            #[cfg(test)]
            value_gate: parking_lot::Mutex::new(None),
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
    pub fn is_subscribed(&self, name: impl AsRef<[u8]>) -> bool {
        self.subs.lock().contains_key(name.as_ref())
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
        served_name: &[u8],
        upstream_name: &[u8],
        asg: Option<String>,
        asl: i32,
    ) -> BridgeResult<()> {
        // The upstream CA client names a channel in text.
        let Ok(upstream_name) = std::str::from_utf8(upstream_name) else {
            return Err(BridgeError::PutRejected(format!(
                "upstream name is not UTF-8: {}",
                String::from_utf8_lossy(upstream_name)
            )));
        };
        let shown = String::from_utf8_lossy(served_name);
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
                pending.insert(served_name.into(), n.clone());
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
                        "upstream subscribe failed (peer creator): {shown}"
                    )))
                };
            }
            Decision::Owner(n) => n,
        };
        // Ensure the pending entry is removed + Notify pulsed even
        // on error / panic paths.
        struct PendingGuard<'a> {
            owner: &'a UpstreamManager,
            key: &'a [u8],
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
        // The receiver is taken with the channel, before its search starts:
        // a connect that wins the race publishes its rights only once.
        let (channel, mut conn_rx) = self.client.create_channel_with_events(upstream_name, 0);
        let channel = Arc::new(channel);

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
        let connected = match channel.wait_connected(self.connect_timeout).await {
            Ok(()) => epics_base_rs::runtime::task::timeout(
                self.connect_timeout,
                connect_rights(&mut conn_rx, || current_rights(&channel)),
            )
            .await
            .ok()
            .flatten()
            .ok_or(CaError::Shutdown),
            Err(e) => Err(e),
        };
        let initial_rights = match connected {
            Ok(rights) => rights,
            Err(e) => {
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
        };

        // Cached mode follows C `activate` → `get(ctrlType)` → `getCB` →
        // `monitor()` + `propMonitor()` (gatePv.cc:398-401, 1693-1706): the
        // shadow PV is registered pending with its hooks and the control
        // get's metadata and both monitors armed, and their first event
        // readies it (`eventCB` `vcAdd`, :1432-1441). The name so answers
        // exists from the connect, as `life()` does (:512-518), and a read
        // before that event waits for it (gateVc.cc:1323-1326). No-cache
        // `activate` adds the vc at once (:397-398), so its PV is ready with
        // a best-effort plain GET's value.
        //
        // The placeholder — a pending PV's value, or a no-cache PV's when that
        // GET fails or times out — takes the *connected* channel's CA native
        // type AND element count (`ca_field_type`/`ca_element_count`), not a
        // hardcoded `Double(0.0)`, so CREATE_CHAN advertises the upstream
        // type (see `native_placeholder`). The channel connected above
        // (`wait_connected`), so this metadata is a cached snapshot read with
        // no round-trip.
        let initial_value = if !self.cache_mode.is_no_cache() {
            channel_placeholder(&channel)
        } else {
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
                    channel_placeholder(&channel)
                }
                Err(_) => {
                    tracing::info!(
                        pv = upstream_name,
                        "ca-gateway-rs: DBR negotiation get timed out; \
                         using upstream native type/count placeholder"
                    );
                    channel_placeholder(&channel)
                }
            }
        };

        // One live ACL cell shared by both hooks AND the subscription
        // record, so an `AS`/`PVL` reload that re-resolves this PV to a
        // new ASG/ASL is observed by every later read/write without
        // rebuilding the closures.
        let acl: PvAclCell = Arc::new(ArcSwap::from_pointee(PvAcl { asg, asl }));
        let hook = build_write_hook(
            served_name.into(),
            upstream_name.to_string(),
            channel.clone(),
            acl.clone(),
            self.write_env.clone(),
            self.reactor.clone(),
        );
        // install the read/write access hook alongside the
        // write hook, capturing the same ACF authority + this PV's
        // live `.pvlist` ASG/ASL, so the CA server gates downstream reads
        // through `can_read` instead of granting every shadow PV a
        // permissive read.
        let access_hook = build_access_hook(acl.clone(), self.write_env.access.clone(), {
            let channel = channel.clone();
            // C `setWriteAccess` (gateVc.cc:1620-1626): a read-only gateway's
            // vc never has write access, so its clients are told so.
            let read_only = self.write_env.read_only;
            move || {
                let mut rights = channel.access_rights();
                rights.write &= !read_only;
                rights
            }
        });
        // The hooks are bound before the PV is ever inserted into
        // `simple_pvs`, so a downstream client cannot race a CREATE_CHAN +
        // WRITE_NOTIFY into a window where the PV is findable but the hook
        // isn't yet bound (which would silently drop the put into the local
        // `pv.set()` fallback).
        //
        // registered under `served_name` (the alias) so a
        // downstream lookup of the alias resolves; the hook above still
        // forwards puts to `upstream_name` (the real PV).
        let pv = epics_base_rs::server::pv::ProcessVariable::new(served_name, initial_value);
        let pv = if self.cache_mode.is_no_cache() {
            pv
        } else {
            pv.pending()
        };
        pv.set_write_hook(hook);
        pv.set_access_hook(access_hook);
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
        // upstream get with no fresh CREATE_CHAN. Cached mode installs no
        // read hook — GETs serve the monitor-fed shadow value. Mirrors
        // `gateVc.cc:1361-1369` forwarding the read to the IOC.
        if self.cache_mode.is_no_cache() {
            let ch = channel.clone();
            pv.set_read_hook(Arc::new(move || {
                let ch = ch.clone();
                Box::pin(async move { ch.get_with_metadata(DbrClass::Time).await })
            }));
        }

        // Seed the shadow PV's DBR_CTRL_* metadata from an initial control
        // get so a downstream DBR_CTRL_*/DBR_GR_* read returns the real
        // units / precision / limits / enum-labels instead of zeroed ones —
        // even before any property *change* occurs. Best-effort and bounded
        // (500 ms): a slow or denied metadata read must not fail the
        // subscription. When it does NOT seed (timeout or error),
        // `seed_succeeded` stays false and the property monitor's FIRST
        // event is consumed to seed instead of being skipped as a redundant
        // confirmation — so a slow initial get never leaves metadata zeroed.
        // Mirrors C `gatePvData::getCB` decoding the initial control get
        // through `runDataCB` → `vc->setPvData(dd)` before it enables
        // `propMonitor()` (gatePv.cc:1688-1705).
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
                Ok(Ok(meta_snap)) => {
                    pv.set_metadata(ctrl_metadata(&meta_snap));
                    true
                }
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

        // Arm the persistent upstream monitors — CACHED mode only.
        //
        // Cached: the value monitor, then the property monitor, as `getCB`
        // calls `monitor()` then `propMonitor()` (gatePv.cc:1703-1705).
        // Autosize (wire count=0) + the configured `-mask` event mask
        // mirrors ca-gateway's `ca_create_subscription(eventType(), 0, chID,
        // GR->eventMask(), ...)` (gatePv.cc:765-774) — count 0 so the IOC
        // reports each event at the record's CURRENT element count, and the
        // configured mask (default DBE_VALUE|DBE_ALARM) rather than the
        // CaChannel default that also requests DBE_LOG. `deadband` is 0.0 —
        // ca-gateway applies no client-side deadband.
        //
        // No-cache: hold NO persistent monitor. The upstream monitors are
        // created lazily by `ensure_monitor` / `ensure_prop_monitor` on the
        // first downstream subscription of each kind (C `getCB` no-cache
        // `needPosting()` gate, gatePv.cc:1737-1753); GETs meanwhile
        // forward fresh via the read hook installed above.
        let monitors: Option<(MonitorHandle, MonitorHandle)> = if self.cache_mode.is_no_cache() {
            None
        } else {
            let armed = async {
                let value = channel
                    .subscribe_with_mask_autosize(0.0, self.event_mask)
                    .await?;
                let property = channel
                    .subscribe_with_mask_autosize(0.0, DBE_PROPERTY)
                    .await?;
                Ok::<_, CaError>((value, property))
            };
            match armed.await {
                Ok(m) => Some(m),
                Err(e) => {
                    return Err(BridgeError::PutRejected(format!("subscribe failed: {e}")));
                }
            }
        };

        // If a prior subscribe attempt left a stale shadow entry (it
        // would have been cleaned up by the failure path, but a hot
        // restart or a crashed task could orphan it), drop it before
        // re-registering. A genuine collision with a non-gateway
        // record/alias surfaces as `Err` and we propagate.
        self.shadow_db.remove_simple_pv(served_name).await;
        if let Err(e) = self.shadow_db.add_simple_pv(pv).await {
            return Err(BridgeError::PutRejected(format!(
                "shadow PV register failed: {e}"
            )));
        }
        // `life()`'s state change (gatePv.cc:536): the name exists from the
        // connect, in both modes; the connect-timeout reaper then leaves it.
        if let Some(entry) = self.cache.read().await.get(served_name) {
            let mut entry = entry.write().await;
            let next = if entry.subscriber_count() > 0 {
                PvState::Active
            } else {
                PvState::Inactive
            };
            entry.set_state(next);
        }

        // When the IOC's access changes (e.g. ASG protection lockout), the
        // C gateway's accessCB (gatePv.cc:1851-1852) calls setWriteAccess +
        // postAccessRights, and postAccessRights loops the downstream
        // channels posting CA_PROTO_ACCESS_RIGHTS (gateVc.cc:1624-1638).
        // The access hook reads the channel, so only the second half is
        // left: when an upstream bit actually flips, fire the downstream
        // access-rights notifier so connected clients re-run the access
        // hook (which ANDs the per-PV ACL with the upstream rights) and
        // re-push CA_PROTO_ACCESS_RIGHTS where the computed level changed.
        // Gated on an observed transition so a no-op callback does not wake
        // every client; the downstream side applies the further
        // `oldaccess != access` filter.
        #[cfg(test)]
        let gate = self.watch_gate.lock().take();
        #[cfg(test)]
        if let Some(gate) = gate {
            let _ = gate.reached.send(channel.clone());
            let _ = gate.release.await;
        }
        // No-cache registered ready; cached waits for the first event.
        let first_life = if self.cache_mode.is_no_cache() {
            ShadowLife::Alive(0)
        } else {
            ShadowLife::Pending(0)
        };
        let (life_tx, life) = tokio::sync::watch::channel(first_life);
        let (first_events, first_rx) = tokio::sync::mpsc::unbounded_channel();
        let channel_events = AbortOnDrop(
            self.reactor.spawn(
                ChannelEventOwner {
                    channel: Arc::downgrade(&channel),
                    name: RecordName::from(served_name),
                    cache: self.cache.clone(),
                    db: self.shadow_db.clone(),
                    life: life_tx,
                    retired: None,
                    notifier: self.access_notifier.clone(),
                    beacon_anomaly: self.write_env.beacon_anomaly.clone(),
                    ctrl_timeout: self.metadata_seed_timeout,
                    shown: initial_rights,
                    cached: !self.cache_mode.is_no_cache(),
                }
                .run(conn_rx, first_rx),
            ),
        );

        // Spawn the forwarding tasks. CACHED: spawn now, seeded with the
        // monitors armed above. NO-CACHE: no task spawns here —
        // `ensure_monitor` / `ensure_prop_monitor` create them
        // (self-subscribing) on the first downstream monitor of each kind,
        // and the release functions abort them when the last one leaves.
        let (task, prop_task) = match monitors {
            Some((value, property)) => (
                Some(self.spawn_forward_task(
                    served_name,
                    channel.clone(),
                    Some(value),
                    life.clone(),
                    first_events.clone(),
                )),
                Some(self.spawn_prop_forward_task(
                    served_name,
                    channel.clone(),
                    Some(property),
                    seed_succeeded,
                    life.clone(),
                    first_events.clone(),
                )),
            ),
            None => (None, None),
        };

        // subscription dedup keys on `served_name` so a
        // repeat search for the same alias is a no-op (fast path above)
        // and a `.pvlist` reload prunes by the served name it admits.
        self.subs.lock().insert(
            served_name.into(),
            UpstreamSubscription {
                channel,
                task,
                prop_task,
                seed_succeeded,
                acl,
                channel_events,
                life,
                first_events,
            },
        );
        Ok(())
    }

    /// Spawn the upstream monitor-forwarding task for `served_name`.
    ///
    /// Within each shadow PV life ([`ChannelEventOwner`] owns the deaths and
    /// revivals) it subscribes (autosize count=0 + configured `-mask` event
    /// mask), forwards every event into the gateway cache + shadow
    /// PvDatabase, and drops the subscription when the life ends, as C
    /// `death` unmonitors and `getCB` re-arms (gatePv.cc:658-661, 1702).
    /// Re-subscribes with exponential backoff. Used by `ensure_subscribed` (cached
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
        served_name: &[u8],
        channel: Arc<CaChannel>,
        initial_monitor: Option<MonitorHandle>,
        mut life: tokio::sync::watch::Receiver<ShadowLife>,
        first_events: FirstEventTx,
    ) -> TaskHandle<()> {
        let cache_clone = self.cache.clone();
        let db_clone = self.shadow_db.clone();
        let stats_for_task = self.write_env.stats.clone();
        // The reconnect / first-subscribe re-arm must use the same
        // autosize (wire count=0) + configured `-mask` event mask as the
        // seeded subscribe, not the DBE_LOG-bearing CaChannel default.
        let event_mask = self.event_mask;
        // the forwarding task addresses the cache entry, shadow PV, and
        // alarm post by `served_name` — the same key the shadow PV and
        // cache were registered under above.
        let name = RecordName::from(served_name);
        #[cfg(test)]
        let gate = self.value_gate.lock().take();
        self.reactor.spawn(async move {
            #[cfg(test)]
            if let Some(release) = gate {
                let _ = release.await;
            }
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            // The monitor the caller subscribed (cached mode) belongs to the
            // first life; `None` (no-cache, or any later life) subscribes at
            // the loop top, the one subscribe site. A task that starts after
            // life 0 ended drops it.
            let mut next_monitor = initial_monitor.filter(|_| life.borrow().armed() == Some(0));
            // The life the live subscription belongs to.
            let mut lived = None;
            loop {
                let Ok(now) = life
                    .wait_for(|l| l.armed().is_some_and(|n| Some(n) != lived))
                    .await
                    .map(|l| l.armed())
                else {
                    return;
                };
                let seeded = next_monitor.take().filter(|_| now == Some(0));
                let mut monitor = match seeded {
                    Some(m) => m,
                    None => match channel.subscribe_with_mask_autosize(0.0, event_mask).await {
                        Ok(m) => m,
                        // `CaError::Shutdown` means the CA client
                        // coordinator is gone — `subscribe()` will never
                        // succeed again, so retrying just spins at the
                        // backoff cap forever. Exit cleanly.
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
                lived = now;
                let ended = loop {
                    let result = tokio::select! {
                        biased;
                        _ = life.wait_for(|l| l.armed() != lived) => break false,
                        result = monitor.recv() => match result {
                            Some(result) => result,
                            None => break true,
                        },
                    };
                    // A CA subscription survives a disconnect: it parks one
                    // `ECA_DISCONN` here and resumes with the circuit. That
                    // death reaches this task through `life`, which drops the
                    // subscription, so an event the server held across an
                    // unresponsive window lands on a cancelled subid. Any
                    // other error is a lost update.
                    let Ok(snapshot) = result else {
                        continue;
                    };

                    stats_for_task.record_event();

                    if let Some(entry_arc) = cache_clone.read().await.get(&name) {
                        entry_arc.write().await.update(snapshot.clone());
                    }

                    // A pending life's first event readies the PV; the next
                    // ones post to it.
                    let Some(snapshot) =
                        offer_first_event(&first_events, &mut life, lived, snapshot, false).await
                    else {
                        continue;
                    };

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
                };
                drop(monitor);
                if ended {
                    // The subscription itself ended within this life —
                    // cancelled, or the CA coordinator shut down. Back off,
                    // then the loop top re-subscribes. Bail out if the cache
                    // entry has been evicted (nobody cares anymore).
                    lived = None;
                    epics_base_rs::runtime::task::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                    if cache_clone.read().await.get(&name).is_none() {
                        return;
                    }
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
    /// Re-subscribes with exponential backoff when its subscription ends, exits
    /// cleanly on `CaError::Shutdown` or cache eviction, and changes no
    /// external truth (aborting only drops the in-memory monitor stream and
    /// leaves the last-installed metadata in place), so
    /// `release_prop_monitor` may abort the returned handle with no
    /// finalizer — symmetric to `spawn_forward_task`, and like it follows
    /// the lives [`ChannelEventOwner`] owns.
    fn spawn_prop_forward_task(
        &self,
        served_name: &[u8],
        channel: Arc<CaChannel>,
        initial_monitor: Option<MonitorHandle>,
        seed_succeeded: bool,
        mut life: tokio::sync::watch::Receiver<ShadowLife>,
        first_events: FirstEventTx,
    ) -> TaskHandle<()> {
        let cache_clone = self.cache.clone();
        let db_clone = self.shadow_db.clone();
        let name = RecordName::from(served_name);
        #[cfg(test)]
        let gate = self.prop_gate.lock().take();
        self.reactor.spawn(async move {
            #[cfg(test)]
            if let Some(release) = gate {
                let _ = release.await;
            }
            let mut backoff = Duration::from_millis(250);
            let max_backoff = Duration::from_secs(30);
            // Life 0's initial event is C's ignored first propEvent
            // (`prop_get_state`, gatePv.cc:203, 1564-1568), which C can ignore
            // because getCB seeds before propMonitor() is enabled
            // (gatePv.cc:1702-1705). When the connect-time get missed its
            // 500 ms the event seeds the metadata instead, but it still never
            // readies the PV: the value event does, as in C. A revival's
            // initial event refreshes, posts and may ready, since C sets
            // `prop_get_state` in the constructor alone. `Some(0)` until life
            // 0's initial event has passed.
            let mut initial = Some(0);
            // Life 0's monitor, as in `spawn_forward_task`.
            let mut next_monitor = initial_monitor.filter(|_| life.borrow().armed() == Some(0));
            // The life the live subscription belongs to. C `death` clears the
            // property monitor with the PV (gatePv.cc:661) and `getCB` re-arms
            // it with the value monitor (gatePv.cc:1703-1705); one kept across
            // the death refreshed a shadow PV that was not there.
            let mut lived = None;
            loop {
                let Ok(now) = life
                    .wait_for(|l| l.armed().is_some_and(|n| Some(n) != lived))
                    .await
                    .map(|l| l.armed())
                else {
                    return;
                };
                let subscribed = match next_monitor.take().filter(|_| now == Some(0)) {
                    Some(m) => Ok(m),
                    None => {
                        channel
                            .subscribe_with_mask_autosize(0.0, DBE_PROPERTY)
                            .await
                    }
                };
                let mut monitor = match subscribed {
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
                lived = now;
                let ended = loop {
                    let result = tokio::select! {
                        biased;
                        _ = life.wait_for(|l| l.armed() != lived) => break false,
                        result = monitor.recv() => match result {
                            Some(result) => result,
                            None => break true,
                        },
                    };
                    // The trigger payload is discarded; a decode error is a
                    // missed trigger, not fatal, and a disconnect is the
                    // death the channel-event owner reports through `life`.
                    if result.is_err() {
                        continue;
                    }
                    let initial_event = initial.take_if(|&mut n| Some(n) == lived).is_some();
                    if initial_event && seed_succeeded {
                        continue;
                    }
                    // A property change fired: re-read the full control
                    // metadata (units/precision/limits/enum-labels + upstream
                    // status/severity) and refresh + fan out downstream.
                    match channel.get_with_metadata(DbrClass::Ctrl).await {
                        Ok(meta_snap) => {
                            // On a pending life it readies the PV instead,
                            // unless it is life 0's initial event: a pending
                            // PV takes that one's metadata and posts nothing.
                            let meta_snap = if initial_event {
                                Some(meta_snap)
                            } else {
                                offer_first_event(&first_events, &mut life, lived, meta_snap, true)
                                    .await
                            };
                            let Some(meta_snap) = meta_snap else {
                                continue;
                            };
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
                };
                drop(monitor);
                if ended {
                    // The subscription itself ended within this life. Back
                    // off, then the top of the loop re-subscribes. Bail if the
                    // cache entry was evicted (nobody cares anymore).
                    lived = None;
                    epics_base_rs::runtime::task::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                    if cache_clone.read().await.get(&name).is_none() {
                        return;
                    }
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
    pub fn ensure_monitor(&self, served_name: &[u8]) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        // Take the channel under the lock, then spawn outside the lock
        // (spawn is sync but keep the critical section minimal), then
        // store the handle under the lock — re-checking no peer raced a
        // task in between.
        let (channel, life, first_events) = {
            let subs = self.subs.lock();
            match subs.get(served_name) {
                Some(sub) if sub.task.is_none() => (
                    sub.channel.clone(),
                    sub.life.clone(),
                    sub.first_events.clone(),
                ),
                // No subscription (evicted) or a task already runs.
                _ => return,
            }
        };
        let handle = self.spawn_forward_task(served_name, channel, None, life, first_events);
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
    pub fn release_monitor(&self, served_name: &[u8]) {
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
    pub fn ensure_prop_monitor(&self, served_name: &[u8]) {
        if !self.cache_mode.is_no_cache() {
            return;
        }
        let (channel, seed_succeeded, life, first_events) = {
            let subs = self.subs.lock();
            match subs.get(served_name) {
                Some(sub) if sub.prop_task.is_none() => (
                    sub.channel.clone(),
                    sub.seed_succeeded,
                    sub.life.clone(),
                    sub.first_events.clone(),
                ),
                // No subscription (evicted) or a prop task already runs.
                _ => return,
            }
        };
        let handle = self.spawn_prop_forward_task(
            served_name,
            channel,
            None,
            seed_succeeded,
            life,
            first_events,
        );
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
    pub fn release_prop_monitor(&self, served_name: &[u8]) {
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
    pub async fn unsubscribe(&self, served_name: &[u8]) {
        let removed = self.subs.lock().remove(served_name);
        if let Some(mut sub) = removed {
            if let Some(task) = sub.task {
                task.abort();
            }
            if let Some(prop_task) = sub.prop_task {
                prop_task.abort();
            }
            // A revival in flight must not re-add the PV removed below.
            sub.channel_events.0.abort();
            let _ = (&mut sub.channel_events.0).await;
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
            .get(upstream_name.as_bytes())
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
            .get(upstream_name.as_bytes())
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
    pub fn asg_for(&self, served_name: &[u8]) -> Option<(Option<String>, i32)> {
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
    pub fn update_acl(&self, served_name: &[u8], asg: Option<String>, asl: i32) -> bool {
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
        let orphans: Vec<RecordName> = self
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
/// `upstream` answers the upstream IOC's `ca_write_access(chID)`/
/// `ca_read_access(chID)` at the time of the call: the channel's own state,
/// not a copy a separate task keeps, so a shadow PV revived from a monitor
/// event already reports every rights change the stream carried ahead of
/// that event (C applies `accessCB` and the event in one callback thread,
/// in stream order). The decisions are
/// `local_acf_write && upstream.write` and `local_acf_read && upstream.read`,
/// matching C `gateVcChan::writeAccess`/`readAccess` (gateVc.cc:341/326):
/// `asclient->writeAccess() && vc->writeAccess()` /
/// `asclient->readAccess() && vc->readAccess()`.
fn build_access_hook(
    acl: PvAclCell,
    access: Arc<ArcSwap<AccessConfig>>,
    upstream: impl Fn() -> AccessRights + Send + Sync + 'static,
) -> epics_base_rs::server::pv::AccessHook {
    Arc::new(move |user: &[u8], host: &[u8]| {
        let cfg = access.load();
        // Load the live `.pvlist` identity each call so a reloaded
        // ASG/ASL is enforced immediately.
        let pv_acl = acl.load();
        let asg_ref = pv_acl.asg.as_deref().unwrap_or("DEFAULT");
        let asl = pv_acl.asl;
        let upstream = upstream();
        let local_read = cfg.can_read(asg_ref, asl, user, host);
        let read = local_read && upstream.read;
        let local_write = if user.is_empty() && cfg.has_rules() {
            false
        } else {
            cfg.can_write(asg_ref, asl, user, host)
        };
        let write = local_write && upstream.write;
        epics_base_rs::server::pv::AccessDecision { read, write }
    })
}

/// ca-gateway `putCB` (gateVc.cc:1519-1533): an upstream put callback
/// that ended ECA_DISCONN or ECA_DISCONNCHID posts canceled; any other
/// failure, dropped, posts -1.
fn put_callback_ended(done: WriteDone, result: CaResult<()>) {
    match result {
        Ok(()) => done.succeed(),
        Err(e) if matches!(e.to_eca_status(CaOp::Write), ECA_DISCONN | ECA_DISCONNCHID) => {
            done.fail(DbStatus::CANCELED_ASYNC_IO)
        }
        Err(_) => drop(done),
    }
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
/// 4. Putlog the attempt: `TrapWrite` (C contract, `gateVc.cc:236`) logs a
///    granted write only when its rule carried `TRAPWRITE`
/// 5. Return the [`WriteForward`] that sends it upstream over the shared
///    channel; `AllWrites` logs its outcome (Ok/Failed) and the put-count
///    stat counts its success.
fn build_write_hook(
    served_name: RecordName,
    pv_name: String,
    channel: Arc<CaChannel>,
    acl: PvAclCell,
    env: WriteHookEnv,
    reactor: epics_base_rs::runtime::task::Reactor,
) -> WriteHook {
    Arc::new(move |write: PvWrite, ctx: WriteContext| {
        let served_name = served_name.clone();
        let pv_name = pv_name.clone();
        let channel = channel.clone();
        let acl = acl.clone();
        let env = env.clone();
        let reactor = reactor.clone();
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
            let (PvWrite::Value(new_value) | PvWrite::AlarmAck(_, new_value)) = &write;
            let value_str = if audit_data {
                format_value_for_audit(new_value, 256)
            } else {
                String::new()
            };

            // Prior cached value for the audit `old=` field. C ca-gateway
            // logs `vc->eventData()` — the virtual connection's cached
            // monitor value — as the put's `old` value
            // (gateResources.cc:486-492). Read once up-front, keyed by
            // `served_name` (the cache key, upstream.rs:232/547), so every
            // `AllWrites` log path (denial branches + forward outcome)
            // records the same pre-put value.
            let old_str = if audit_data {
                cached_old_for_audit(&env, &served_name).await
            } else {
                String::new()
            };

            // 1. read-only mode — gateway-wide flag. Every refusal here is
            // `write()`'s S_cas_noWrite (gateVc.cc:1463).
            if env.read_only {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(DbStatus::CAS_NO_WRITE);
            }

            // 2. pvlist host-based DENY. `load_full` is wait-free.
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
                return Err(DbStatus::CAS_NO_WRITE);
            };
            if pvlist.is_host_denied(&pv_name, &policy_host) {
                env.stats.record_readonly_reject();
                log_denial(&env, &ctx, &pv_name, &value_str, &old_str).await;
                return Err(DbStatus::CAS_NO_WRITE);
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
                return Err(DbStatus::CAS_NO_WRITE);
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
                return Err(DbStatus::CAS_NO_WRITE);
            }

            // 4. C logs the trapped attempt before the write
            // (gateVc.cc:236-263).
            emit_putlog(
                &env,
                &ctx,
                &pv_name,
                &value_str,
                &old_str,
                WriteAudit::Attempted { trap: permit.trap },
            )
            .await;

            // 5. Forward upstream as the client asked: a plain write with
            // `ca_array_put`, a put callback with `ca_array_put_callback`,
            // answered when upstream answers (gatePv.cc:1100-1135,
            // gateVc.cc:1497-1514). An alarm acknowledgement goes as the
            // request type it came in (gatePv.cc:1033-1039). Nothing here
            // waits on upstream.
            let forward: WriteForward = Box::new(move |done| {
                let outcome = SentWrite {
                    env,
                    ctx,
                    pv_name,
                    value_str,
                    old_str,
                };
                let ack_type = |ack| match ack {
                    AlarmAck::Transient => DBR_PUT_ACKT,
                    AlarmAck::Severity => DBR_PUT_ACKS,
                };
                let Some(done) = done else {
                    let sent = match &write {
                        PvWrite::Value(value) => channel.put_no_callback(value),
                        PvWrite::AlarmAck(ack, levels) => {
                            channel.put_as_dbr_no_callback(ack_type(*ack), levels)
                        }
                    };
                    reactor.spawn(outcome.record(sent.is_ok()));
                    return sent;
                };
                match &write {
                    PvWrite::Value(value) => answer_when_upstream_does(
                        channel.put_callback(value),
                        outcome,
                        done,
                        &reactor,
                    ),
                    PvWrite::AlarmAck(ack, levels) => answer_when_upstream_does(
                        channel.put_as_dbr_callback(ack_type(*ack), levels),
                        outcome,
                        done,
                        &reactor,
                    ),
                }
            });
            Ok(forward)
        })
    })
}

/// Answer `done` when the put callback `issued` sent upstream completes.
fn answer_when_upstream_does(
    issued: CaResult<impl std::future::Future<Output = CaResult<()>> + Send + 'static>,
    outcome: SentWrite,
    done: WriteDone,
    reactor: &epics_base_rs::runtime::task::Reactor,
) -> CaResult<()> {
    let completion = match issued {
        Ok(completion) => completion,
        Err(e) => {
            reactor.spawn(outcome.record(false));
            return Err(e);
        }
    };
    reactor.spawn(async move {
        let result = completion.await;
        outcome.record(result.is_ok()).await;
        put_callback_ended(done, result);
    });
    Ok(())
}

/// A write the hook sent upstream, for the put log and the put count once
/// its outcome is known.
struct SentWrite {
    env: WriteHookEnv,
    ctx: WriteContext,
    pv_name: String,
    value_str: String,
    old_str: String,
}

impl SentWrite {
    async fn record(self, ok: bool) {
        emit_putlog(
            &self.env,
            &self.ctx,
            &self.pv_name,
            &self.value_str,
            &self.old_str,
            WriteAudit::Forwarded { ok },
        )
        .await;
        if ok {
            self.env.stats.record_put();
        }
    }
}

/// Read the gateway's last-cached upstream value for `key` and render it
/// for the put-audit `old=` field. Mirrors C ca-gateway, which logs
/// `vc->eventData()` (the virtual connection's cached monitor value) as
/// the put's `old` value (gateResources.cc:486-492). `key` is the
/// `served_name` the cache and forwarding task register under
/// (upstream.rs:547/944). Returns `"?"` when no monitor update has
/// populated the cache yet, matching C's `old_value == NULL` →
/// `acOldVal = "?"` (gateResources.cc:476-480).
async fn cached_old_for_audit(env: &WriteHookEnv, key: &[u8]) -> String {
    let entry = env.cache.read().await.get(key);
    if let Some(entry) = entry
        && let Some(snap) = &entry.read().await.cached
    {
        return format_value_for_audit(&snap.value, 256);
    }
    "?".to_string()
}

/// What happened to a client write, for the put-audit decision. The
/// rejection branches report [`Self::Denied`]; a granted write reports
/// [`Self::Attempted`] with the matched rule's TRAPWRITE mask before it is
/// sent, then [`Self::Forwarded`] with the upstream outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteAudit {
    /// The gateway rejected the put before forwarding (read-only mode,
    /// host deny, missing identity, or ACF deny).
    Denied,
    /// The gateway is about to forward the put upstream. `trap` is the
    /// matched WRITE rule's `TRAPWRITE` mask.
    Attempted { trap: bool },
    /// The forwarded put's outcome: whether upstream took it.
    Forwarded { ok: bool },
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
/// write logs nothing, and a trapped grant logs its attempt, before the
/// write. `AllWrites` is the broader fail-loud superset: every denial and
/// every outcome logs, tagged.
fn putlog_outcome(scope: PutLogScope, audit: WriteAudit) -> Option<PutLogDecision> {
    match (scope, audit) {
        // Broader audit: log every event with its outcome token.
        (PutLogScope::AllWrites, WriteAudit::Denied) => {
            Some(PutLogDecision::AllWrites(PutOutcome::Denied))
        }
        (PutLogScope::AllWrites, WriteAudit::Attempted { .. }) => None,
        (PutLogScope::AllWrites, WriteAudit::Forwarded { ok }) => {
            Some(PutLogDecision::AllWrites(if ok {
                PutOutcome::Ok
            } else {
                PutOutcome::Failed
            }))
        }
        // C contract: denials and non-trapped writes are never logged; a
        // trapped grant logs as the valueless C default line.
        (PutLogScope::TrapWrite, WriteAudit::Denied) => None,
        (PutLogScope::TrapWrite, WriteAudit::Attempted { trap: true }) => {
            Some(PutLogDecision::TrapWrite)
        }
        (PutLogScope::TrapWrite, WriteAudit::Attempted { trap: false }) => None,
        (PutLogScope::TrapWrite, WriteAudit::Forwarded { .. }) => None,
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
            truncated = EpicsValue::ShortArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        EpicsValue::FloatArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::FloatArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        EpicsValue::EnumArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::EnumArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        EpicsValue::DoubleArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::DoubleArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        EpicsValue::LongArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::LongArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        EpicsValue::CharArray(arr) if arr.len() > max_len => {
            truncated = EpicsValue::CharArray(arr.head(max_len));
            &truncated
        }
        EpicsValue::StringArray(arr) if arr.len() > HEAD_PEEK_ELEMS => {
            truncated = EpicsValue::StringArray(arr.head(HEAD_PEEK_ELEMS));
            &truncated
        }
        _ => v,
    };
    let s = v_for_format.text_g();
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
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `gdd_to_log_string` prints a real as `%g` (gateResources.cc:229).
    #[test]
    fn an_audited_real_is_c_percent_g() {
        assert_eq!(
            format_value_for_audit(&EpicsValue::Double(0.1 + 0.2), 256),
            "0.3"
        );
        assert_eq!(
            format_value_for_audit(&EpicsValue::DoubleArray(vec![1e20, 0.5].into()), 256),
            "[1e+20, 0.5]"
        );
    }
    // Only the eight gated tests below host a real async CA server, so the
    // import carries their predicate — otherwise it is unused on the exec
    // backend.
    #[cfg(tokio_backend)]
    use epics_ca_rs::server::CaServer;
    // Same predicate as the import above: every `#[serial(epics_env)]` test in
    // this module is one of the gated ones.
    #[cfg(tokio_backend)]
    use serial_test::serial;

    /// `connect_rights` takes the last rights before `Connected`, and across
    /// its lag boundary. Eighteen events into the
    /// 16-slot receiver drop the first pair, and the oldest kept pair is a
    /// past connect's — so a lagged read must take the resync, not that.
    #[tokio::test]
    async fn connect_rights_resyncs_a_lagged_receiver() {
        use ConnectionEvent::*;
        let rights = |read, write| AccessRights { read, write };
        let send_all = |tx: &broadcast::Sender<ConnectionEvent>, events: &[ConnectionEvent]| {
            for e in events {
                tx.send(*e).expect("receiver alive");
            }
        };
        let bound = Duration::from_secs(1);
        let granted = AccessRightsChanged {
            read: true,
            write: true,
        };

        // No lag, no resync: the last rights before `Connected`, never a pair
        // from before a `Disconnected` nor one after `Connected`.
        let read_only = AccessRightsChanged {
            read: true,
            write: false,
        };
        let none = AccessRightsChanged {
            read: false,
            write: false,
        };
        for (events, want) in [
            ([granted, Disconnected, read_only, Connected], (true, false)),
            ([Disconnected, none, Connected, read_only], (false, false)),
        ] {
            let (tx, mut rx) = broadcast::channel(16);
            send_all(&tx, &events);
            let got = tokio::time::timeout(
                bound,
                connect_rights(&mut rx, || async { panic!("no lag, no resync") }),
            )
            .await
            .expect("bounded");
            assert_eq!(got.map(|r| (r.read, r.write)), Some(want), "{events:?}");
        }

        // `Connected` with no rights since `Disconnected`: the resync's, not
        // the grant from before the outage.
        let (tx, mut rx) = broadcast::channel(16);
        send_all(&tx, &[granted, Disconnected, Connected]);
        let got = tokio::time::timeout(
            bound,
            connect_rights(&mut rx, || async { Some(rights(true, false)) }),
        )
        .await
        .expect("bounded");
        assert_eq!(got.map(|r| (r.read, r.write)), Some((true, false)));

        // Lagged while connected: the resync's rights.
        let (tx, mut rx) = broadcast::channel(16);
        let mut flap = vec![Connected, granted];
        for _ in 0..7 {
            flap.extend([Connected, granted]);
        }
        flap.extend([
            Connected,
            AccessRightsChanged {
                read: true,
                write: false,
            },
        ]);
        assert_eq!(flap.len(), 18);
        send_all(&tx, &flap);
        let got = tokio::time::timeout(
            bound,
            connect_rights(&mut rx, || async { Some(rights(true, false)) }),
        )
        .await
        .expect("bounded");
        assert_eq!(
            got.map(|r| (r.read, r.write)),
            Some((true, false)),
            "not a past connect's grant"
        );

        // Lagged while disconnected: the next connect's pair.
        let (tx, mut rx) = broadcast::channel(16);
        send_all(&tx, &[Disconnected; 18]);
        let next = tokio::spawn({
            let tx = tx.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                send_all(
                    &tx,
                    &[
                        AccessRightsChanged {
                            read: false,
                            write: false,
                        },
                        Connected,
                    ],
                );
            }
        });
        let got = tokio::time::timeout(bound, connect_rights(&mut rx, || async { None }))
            .await
            .expect("bounded");
        next.await.expect("sender");
        assert_eq!(got.map(|r| (r.read, r.write)), Some((false, false)));
    }

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
        // C-compatible line for the attempt, logged before the write.
        assert_eq!(
            putlog_outcome(TrapWrite, WriteAudit::Attempted { trap: true }),
            Some(PutLogDecision::TrapWrite),
            "TRAPWRITE grant must log a C-style (valueless, token-less) line"
        );
        // The upstream outcome adds nothing to the C line, either way.
        for ok in [true, false] {
            assert_eq!(
                putlog_outcome(TrapWrite, WriteAudit::Forwarded { ok }),
                None,
                "the attempt is the C line; its outcome is not logged"
            );
        }

        // A granted write to a non-TRAPWRITE rule → no line.
        assert_eq!(
            putlog_outcome(TrapWrite, WriteAudit::Attempted { trap: false }),
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
        // AllWrites is the broader fail-loud audit: every denial and every
        // outcome logs with its token, independent of the trap mask.
        use PutLogScope::AllWrites;
        assert_eq!(
            putlog_outcome(AllWrites, WriteAudit::Denied),
            Some(PutLogDecision::AllWrites(PutOutcome::Denied))
        );
        assert_eq!(
            putlog_outcome(AllWrites, WriteAudit::Attempted { trap: true }),
            None,
            "AllWrites logs the outcome, not the attempt"
        );
        assert_eq!(
            putlog_outcome(AllWrites, WriteAudit::Forwarded { ok: true }),
            Some(PutLogDecision::AllWrites(PutOutcome::Ok)),
            "AllWrites logs a success as OK"
        );
        assert_eq!(
            putlog_outcome(AllWrites, WriteAudit::Forwarded { ok: false }),
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
        assert_eq!(cached_old_for_audit(&env, b"GW:MISS").await, "?");

        // (b) entry exists but no monitor value cached yet → "?".
        env.cache.write().await.get_or_create("GW:PV");
        assert_eq!(cached_old_for_audit(&env, b"GW:PV").await, "?");

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
        assert_eq!(cached_old_for_audit(&env, b"GW:PV").await, "24.8");
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
    #[cfg(tokio_backend)]
    fn dead_upstream() -> (std::net::UdpSocket, u16) {
        let sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("own a dead CA port");
        let port = sock.local_addr().unwrap().port();
        (sock, port)
    }

    /// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port` so the
    /// `UpstreamManager`'s internal env-driven `CaClient::new()` connects
    /// to the test server. Callers must be `#[serial(epics_env)]`.
    #[cfg(tokio_backend)]
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

    /// The connect budget for a manager whose upstream is a hosted server
    /// that answers: none, so `ensure_subscribed` waits on the connect itself.
    /// The 1 s policy deadline is a wall clock a loaded gate host outruns, and
    /// only the tests about that deadline (a dead upstream) may run under it.
    #[cfg(tokio_backend)]
    const NO_CONNECT_DEADLINE: Duration = Duration::MAX;

    /// Build an `UpstreamManager` whose internal `CaClient::new()` reads
    /// the ambient `EPICS_CA_*` env — call [`pin_env`] first so the client
    /// is pinned to the test server.
    #[cfg(tokio_backend)]
    async fn pinned_manager(db: Arc<PvDatabase>) -> UpstreamManager {
        pinned_manager_full(
            db,
            Arc::new(RwLock::new(PvCache::new())),
            CacheMode::Cached,
            NO_CONNECT_DEADLINE,
        )
        .await
    }

    /// Like [`pinned_manager`] but with a caller-supplied cache (so a test
    /// can inspect per-PV state) and an explicit cache mode (so a test can
    /// exercise the no-cache GET-forward + lazy-monitor paths).
    #[cfg(tokio_backend)]
    async fn pinned_manager_full(
        db: Arc<PvDatabase>,
        cache: Arc<RwLock<PvCache>>,
        cache_mode: CacheMode,
        connect_timeout: Duration,
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
            connect_timeout,
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
    #[cfg(tokio_backend)]
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
    #[cfg(tokio_backend)]
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
    /// The upstream rights the access hook reads, from two test-held flags.
    fn upstream_flags(
        write: &Arc<AtomicBool>,
        read: &Arc<AtomicBool>,
    ) -> impl Fn() -> AccessRights + Send + Sync + 'static {
        let (write, read) = (write.clone(), read.clone());
        move || AccessRights {
            read: read.load(Ordering::Relaxed),
            write: write.load(Ordering::Relaxed),
        }
    }

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
        let hook = build_access_hook(acl, access, upstream_flags(&upstream_write, &upstream_read));

        // Privileged user: read granted, write denied (no WRITE rule).
        let alice = hook(b"alice", b"host1");
        assert!(alice.read, "alice is in UAG(ops) → READ");
        assert!(!alice.write, "no WRITE rule → write denied even for alice");

        // Unprivileged user: read AND write denied — this is the gap
        // FR-1 closes (previously every shadow PV read was permissive).
        let intruder = hook(b"intruder", b"host1");
        assert!(!intruder.read, "intruder is not in UAG(ops) → no READ");
        assert!(!intruder.write, "intruder → no WRITE");

        // No client identity: read evaluates normally (denied here, no
        // matching UAG), write is force-denied while rules are loaded —
        // mirrors the write hook's empty-user guard.
        let anon = hook(b"", b"host1");
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
        let hook = build_access_hook(acl, access, upstream_flags(&upstream_write, &upstream_read));
        let d = hook(b"anyone", b"anywhere");
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
        let hook = build_access_hook(acl, access, upstream_flags(&upstream_write, &upstream_read));

        let d = hook(b"alice", b"host1");
        assert!(d.read, "read must still be granted by allow-all");
        assert!(
            !d.write,
            "upstream write-denied must override local allow-all"
        );

        // Restoring upstream write-access restores write permission.
        upstream_write.store(true, Ordering::Relaxed);
        let d2 = hook(b"alice", b"host1");
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
        let hook = build_access_hook(acl, access, upstream_flags(&upstream_write, &upstream_read));

        let d = hook(b"alice", b"host1");
        assert!(
            !d.read,
            "upstream read-denied must override local allow-all"
        );
        assert!(d.write, "write must still be granted by allow-all");

        // Restoring upstream read-access restores read permission.
        upstream_read.store(true, Ordering::Relaxed);
        let d2 = hook(b"alice", b"host1");
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
        let hook = build_access_hook(
            acl.clone(),
            access,
            upstream_flags(&upstream_write, &upstream_read),
        );

        // Under OldGroup, alice has READ; bob does not.
        assert!(hook(b"alice", b"h").read, "OldGroup grants alice READ");
        assert!(!hook(b"bob", b"h").read, "OldGroup denies bob READ");

        // Simulate the reload: swap the live ACL cell the hook holds.
        acl.store(Arc::new(PvAcl {
            asg: Some("NewGroup".to_string()),
            asl: 1,
        }));

        // The same hook now computes against NewGroup: rights flip.
        assert!(
            !hook(b"alice", b"h").read,
            "after live ACL swap to NewGroup, alice's READ is revoked"
        );
        assert!(
            hook(b"bob", b"h").read,
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
    #[cfg(tokio_backend)]
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

        mgr.ensure_subscribed(
            name.as_bytes(),
            name.as_bytes(),
            Some("OldGroup".to_string()),
            1,
        )
        .await
        .expect("ensure_subscribed connects to the hosted upstream");
        assert_eq!(
            mgr.asg_for(name.as_bytes()),
            Some((Some("OldGroup".to_string()), 1)),
            "initial ASG/ASL is the value resolved at subscribe"
        );

        // Reload moves the still-admitted PV to a new ASG/ASL.
        assert!(
            mgr.update_acl(name.as_bytes(), Some("NewGroup".to_string()), 0),
            "a real ASG/ASL change reports true"
        );
        assert_eq!(
            mgr.asg_for(name.as_bytes()),
            Some((Some("NewGroup".to_string()), 0)),
            "asg_for follows the reload immediately"
        );

        // Idempotent: re-applying the identical identity is a no-op.
        assert!(
            !mgr.update_acl(name.as_bytes(), Some("NewGroup".to_string()), 0),
            "an unchanged ASG/ASL reports false"
        );

        mgr.shutdown().await;
    }

    /// An upstream rights change that lands after the connect-time seed but
    /// before the watcher reads its first event still reaches the shadow
    /// PV's access hook — the decision the downstream server pushes to its
    /// clients. `ensure_subscribed` is held at that point while the upstream
    /// revokes WRITE and the channel's own state shows the change published,
    /// so a watcher that subscribes only now has already missed it.
    // Same in-process `CaServer` gate as the test above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn an_upstream_write_revoked_after_the_connect_reaches_the_access_hook() {
        let bound = Duration::from_secs(5);
        let name = "AS:revoke:pv";
        let dir = tempfile::tempdir().expect("temp");
        let acf = dir.path().join("rights.acf");
        std::fs::write(&acf, "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\n")
            .expect("write acf");
        let server = Arc::new(
            CaServer::builder()
                .port(0)
                .pv(name, EpicsValue::Double(1.0))
                .acf_file(acf.to_str().unwrap())
                .expect("load acf")
                .build()
                .await
                .expect("CA server"),
        );
        let port = server.udp_port();
        let _server = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = Arc::new(pinned_manager(db.clone()).await);
        let (reached, reached_rx) = epics_base_rs::runtime::sync::oneshot::channel();
        let (release_tx, release) = epics_base_rs::runtime::sync::oneshot::channel();
        *mgr.watch_gate.lock() = Some(WatchGate { reached, release });
        let subscribe = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
                    .await
            }
        });

        let channel = tokio::time::timeout(bound, reached_rx)
            .await
            .expect("ensure_subscribed reaches the watcher")
            .expect("gate sender kept");
        let hook = db
            .find_pv(name)
            .expect("the shadow PV is registered before the watcher")
            .access_hook()
            .expect("shadow PV carries the access hook");
        assert!(
            hook(b"alice", b"host1").write,
            "the ACF grants WRITE at connect"
        );

        std::fs::write(&acf, "ASG(DEFAULT) {\n RULE(1, READ)\n}\n").expect("rewrite acf");
        server.reload_acf().await.expect("reload acf");
        let deadline = std::time::Instant::now() + bound;
        while channel.info().await.expect("connected").access_rights.write {
            assert!(
                std::time::Instant::now() < deadline,
                "the upstream channel never saw WRITE revoked"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release_tx.send(()).expect("ensure_subscribed is waiting");
        tokio::time::timeout(bound, subscribe)
            .await
            .expect("ensure_subscribed returns")
            .expect("join")
            .expect("ensure_subscribed connects to the hosted upstream");

        let deadline = std::time::Instant::now() + bound;
        while hook(b"alice", b"host1").write {
            assert!(
                std::time::Instant::now() < deadline,
                "the gateway still grants a write the upstream revoked"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(hook(b"alice", b"host1").read, "READ is still granted");

        mgr.shutdown().await;
    }

    /// A read-only gateway tells its clients they cannot write even where the
    /// upstream and the ACF both grant it — C `setWriteAccess`
    /// (gateVc.cc:1620-1626) — so a write is refused by the access check; its
    /// `write()` refuses one anyway with S_cas_noWrite (gateVc.cc:1463).
    // Same in-process `CaServer` gate as the test above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_read_only_gateway_grants_no_write() {
        let name = "RO:gw:pv";
        let server = Arc::new(
            CaServer::builder()
                .port(0)
                .pv(name, EpicsValue::Double(1.0))
                .build()
                .await
                .expect("CA server"),
        );
        let port = server.udp_port();
        let _server = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });

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
            read_only: true,
            cache_mode: CacheMode::Cached,
            connect_timeout: NO_CONNECT_DEADLINE,
            event_mask: crate::ca_gateway::server::DEFAULT_EVENT_MASK,
            beacon_anomaly: env.beacon_anomaly.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        })
        .await
        .expect("manager builds");
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        let pv = db.find_pv(name).expect("the shadow PV is registered");
        let hook = pv.access_hook().expect("shadow PV carries the access hook");
        let rights = hook(b"alice", b"host1");
        assert!(rights.read, "READ is granted");
        assert!(!rights.write, "a read-only gateway grants no WRITE");
        let write = pv.write_hook().expect("shadow PV carries the write hook");
        assert_eq!(
            write(EpicsValue::Double(2.0).into(), WriteContext::default())
                .await
                .err(),
            Some(DbStatus::CAS_NO_WRITE)
        );

        mgr.shutdown().await;
    }

    /// An upstream rights change on a live circuit, outside any window,
    /// reaches a downstream client as CA_PROTO_ACCESS_RIGHTS — C accessCB
    /// → postAccessRights (gatePv.cc:1851-1852, gateVc.cc:1624-1638). The
    /// watcher wakes on the upstream event and reads the channel's rights,
    /// so the event must not be announced before those rights are readable.
    // Same in-process `CaServer` gate as the test above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn an_upstream_rights_change_reaches_a_downstream_client() {
        use epics_ca_rs::client::ConnectionEvent;
        let bound = Duration::from_secs(5);
        let name = "AS:live:pv";
        let dir = tempfile::tempdir().expect("temp");
        let acf = dir.path().join("rights.acf");
        std::fs::write(&acf, "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\n")
            .expect("write acf");
        let upstream = Arc::new(
            CaServer::builder()
                .port(0)
                .pv(name, EpicsValue::Double(1.0))
                .acf_file(acf.to_str().unwrap())
                .expect("load acf")
                .build()
                .await
                .expect("upstream CA server"),
        );
        pin_env(upstream.udp_port());
        let _upstream = tokio::spawn({
            let server = upstream.clone();
            async move { server.run().await }
        });
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;
        tokio::time::timeout(
            bound,
            mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0),
        )
        .await
        .expect("ensure_subscribed returns")
        .expect("ensure_subscribed connects to the hosted upstream");

        let downstream = Arc::new(
            CaServer::from_parts(
                db.clone(),
                0,
                None,
                epics_base_rs::server::access_security::new_acf_cell(None),
                None,
                None,
            )
            .await
            .expect("downstream CA server"),
        );
        mgr.install_access_notifier(downstream.access_rights_notifier());
        // The upstream client already holds its address list; this one is
        // the downstream client's.
        pin_env(downstream.udp_port());
        let _downstream = tokio::spawn({
            let server = downstream.clone();
            async move { server.run().await }
        });
        let client = CaClient::new().await.expect("downstream client");
        let (_ch, mut rx) = client.create_channel_with_events(name, 0);
        let mut next = async || {
            tokio::time::timeout(bound, rx.recv())
                .await
                .expect("downstream connection event inside the bound")
                .expect("receiver open, not lagged")
        };
        assert_eq!(
            next().await,
            ConnectionEvent::AccessRightsChanged {
                read: true,
                write: true
            }
        );
        assert_eq!(next().await, ConnectionEvent::Connected);

        std::fs::write(&acf, "ASG(DEFAULT) {\n RULE(1, READ)\n}\n").expect("rewrite acf");
        upstream.reload_acf().await.expect("reload acf");
        assert_eq!(
            next().await,
            ConnectionEvent::AccessRightsChanged {
                read: true,
                write: false
            },
            "the downstream client was never told WRITE was revoked"
        );

        mgr.shutdown().await;
    }

    /// A gateway whose upstream PV `held` reports each write it receives:
    /// `Some(done)` for a put callback, which completes when the test fires
    /// `done`, `None` for a plain write. `other` is a plain upstream PV.
    #[cfg(tokio_backend)]
    async fn gateway_over_a_held_upstream(
        held: &str,
        other: &str,
    ) -> (
        CaClient,
        tokio::sync::mpsc::UnboundedReceiver<(PvWrite, Option<WriteDone>)>,
        Vec<tokio::task::JoinHandle<()>>,
        UpstreamManager,
    ) {
        let upstream = Arc::new(
            CaServer::builder()
                .port(0)
                .pv(other, EpicsValue::Double(2.0))
                .build()
                .await
                .expect("upstream CA server"),
        );
        let (writes, received) = tokio::sync::mpsc::unbounded_channel();
        let hook: WriteHook = Arc::new(move |write, _ctx| {
            let writes = writes.clone();
            Box::pin(async move {
                let report: WriteForward = Box::new(move |done| {
                    let _ = writes.send((write, done));
                    Ok(())
                });
                Ok(report)
            })
        });
        upstream
            .database()
            .add_pv_with_hook(held, EpicsValue::Double(1.0), hook)
            .await
            .expect("held upstream PV");
        pin_env(upstream.udp_port());
        let mut tasks = vec![tokio::spawn({
            let server = upstream.clone();
            async move {
                let _ = server.run().await;
            }
        })];
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;
        for name in [held, other] {
            mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
                .await
                .expect("upstream connects");
            let pv = db.find_pv(name).expect("registered");
            while pv.is_pending() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let downstream = Arc::new(
            CaServer::from_parts(
                db.clone(),
                0,
                None,
                epics_base_rs::server::access_security::new_acf_cell(None),
                None,
                None,
            )
            .await
            .expect("downstream CA server"),
        );
        pin_env(downstream.udp_port());
        tasks.push(tokio::spawn(async move {
            let _ = downstream.run().await;
        }));
        let client = CaClient::new().await.expect("downstream client");
        (client, received, tasks, mgr)
    }

    /// `putCB` posts canceled for ECA_DISCONN and ECA_DISCONNCHID, and -1
    /// for any other failure (gateVc.cc:1519-1533).
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn an_upstream_put_callback_posts_what_put_cb_does() {
        use epics_base_rs::server::database::channel_io::{self, ChannelTarget, LongStringMode};
        use epics_base_rs::server::pv::PutStarted;
        use epics_ca_rs::protocol::ECA_PUTFAIL;
        let db = PvDatabase::new();
        let (dones, mut taken) = tokio::sync::mpsc::unbounded_channel();
        let hook: WriteHook = Arc::new(move |_value, _ctx| {
            let dones = dones.clone();
            Box::pin(async move {
                let forward: WriteForward = Box::new(move |done| {
                    let _ = dones.send(done);
                    Ok(())
                });
                Ok(forward)
            })
        });
        db.add_pv_with_hook("PCB:pv", EpicsValue::Double(0.0), hook)
            .await
            .expect("pv");
        let target = ChannelTarget::SimplePv(db.find_pv("PCB:pv").expect("pv"));
        for (result, posted) in [
            (Ok(()), Ok(())),
            (Err(CaError::Disconnected), Err(DbStatus::CANCELED_ASYNC_IO)),
            (
                Err(CaError::ServerError(ECA_DISCONNCHID)),
                Err(DbStatus::CANCELED_ASYNC_IO),
            ),
            (
                Err(CaError::WriteFailed(ECA_PUTFAIL)),
                Err(DbStatus::FAILED),
            ),
        ] {
            let put = channel_io::put(
                &db,
                &target,
                LongStringMode::Plain,
                EpicsValue::Double(1.0),
                true,
                WriteContext::default,
            );
            let Ok(PutStarted::Posted(end)) = put.await else {
                panic!("posted");
            };
            let done = taken.recv().await.flatten().expect("a put callback");
            put_callback_ended(done, result);
            assert_eq!(end.await, posted);
        }
    }

    /// A put callback forwarded upstream does not hold the client's circuit
    /// while upstream works on it, a second one on the same channel
    /// included: PCAS answers `writeNotify` later (S_casApp_asyncCompletion,
    /// gateVc.cc:1497-1511) and keeps serving.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_put_callback_held_upstream_leaves_the_circuit_serving() {
        let bound = Duration::from_secs(10);
        let (client, mut received, _tasks, mgr) =
            gateway_over_a_held_upstream("WN:held", "WN:other").await;
        let held = client.create_channel("WN:held");
        let other = client.create_channel("WN:other");
        held.wait_connected(bound).await.expect("held connects");
        other.wait_connected(bound).await.expect("other connects");

        let first = held.put_callback(&EpicsValue::Double(3.0)).expect("sent");
        let second = held.put_callback(&EpicsValue::Double(4.0)).expect("sent");
        let mut dones = Vec::new();
        for _ in 0..2 {
            let done = tokio::time::timeout(bound, received.recv())
                .await
                .expect("upstream got the put")
                .expect("hook alive")
                .1
                .expect("a put callback upstream");
            dones.push(done);
        }

        let (_, value) = tokio::time::timeout(bound, other.get())
            .await
            .expect("the other channel answers while the puts are held")
            .expect("get");
        assert_eq!(value, EpicsValue::Double(2.0));

        for done in dones {
            done.succeed();
        }
        for put in [first, second] {
            tokio::time::timeout(bound, put)
                .await
                .expect("answered once upstream completes")
                .expect("ECA_NORMAL");
        }
        mgr.shutdown().await;
    }

    /// A plain downstream WRITE reaches upstream as a plain WRITE
    /// (`ca_array_put`, gatePv.cc:1131), never as a put callback.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_plain_write_goes_upstream_as_a_plain_write() {
        let bound = Duration::from_secs(10);
        let (client, mut received, _tasks, mgr) =
            gateway_over_a_held_upstream("WP:held", "WP:other").await;
        let held = client.create_channel("WP:held");
        held.wait_connected(bound).await.expect("held connects");

        held.put_no_callback(&EpicsValue::Double(3.0))
            .expect("sent");
        let (_, done) = tokio::time::timeout(bound, received.recv())
            .await
            .expect("upstream got the write")
            .expect("hook alive");
        assert!(done.is_none(), "upstream saw a put callback");
        mgr.shutdown().await;
    }

    /// An alarm acknowledgement reaches upstream as the request type it came
    /// in, plain or as a put callback (gateVc.cc:1459-1460, gatePv.cc:1033-1039).
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn an_alarm_acknowledgement_goes_upstream_as_one() {
        let bound = Duration::from_secs(10);
        let (client, mut received, _tasks, mgr) =
            gateway_over_a_held_upstream("AK:held", "AK:other").await;
        let held = client.create_channel("AK:held");
        held.wait_connected(bound).await.expect("held connects");

        held.put_as_dbr_no_callback(DBR_PUT_ACKT, &EpicsValue::Short(1))
            .expect("sent");
        let (write, done) = tokio::time::timeout(bound, received.recv())
            .await
            .expect("upstream got the acknowledgement")
            .expect("hook alive");
        assert_eq!(
            write,
            PvWrite::AlarmAck(AlarmAck::Transient, EpicsValue::Short(1))
        );
        assert!(done.is_none(), "upstream saw a put callback");

        let acked = held
            .put_as_dbr_callback(DBR_PUT_ACKS, &EpicsValue::Short(2))
            .expect("sent");
        let (write, done) = tokio::time::timeout(bound, received.recv())
            .await
            .expect("upstream got the acknowledgement")
            .expect("hook alive");
        assert_eq!(
            write,
            PvWrite::AlarmAck(AlarmAck::Severity, EpicsValue::Short(2))
        );
        done.expect("a put callback upstream").succeed();
        tokio::time::timeout(bound, acked)
            .await
            .expect("answered once upstream completes")
            .expect("ECA_NORMAL");
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
    /// it. Skipping life 0's initial event whatever `seed_succeeded` says
    /// leaves units empty and fails this test.
    // Stands up an in-process async `CaServer`, whose accept/search
    // loops reach the network through the `runtime::task` seam. Under
    // this feature that seam is the std-thread executor, which starts no
    // tokio reactor, so the server's first `tokio::net` call panics on a
    // `cbMedium` worker and the upstream never connects. Same reason as
    // `epics-ca-rs`'s `two_priorities_open_two_circuits`.
    #[cfg(tokio_backend)]
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
        let (release_prop, release) = epics_base_rs::runtime::sync::oneshot::channel();
        *mgr.prop_gate.lock() = Some(release);

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        // Precondition: the seed genuinely MISSED — the shadow's units must
        // be empty while the prop task is held before it subscribes.
        // Without this guard the test would pass vacuously if the seed ever
        // silently succeeded (the units would already be present).
        {
            let pv = db.find_pv(name).expect("shadow registered");
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

        release_prop.send(()).expect("the prop task is held");

        // The cached property task subscribes DBE_PROPERTY, receives its
        // initial-state event, and — because the seed missed — consumes it
        // to seed metadata (get_with_metadata → post_pv_property). Poll the
        // shadow until its display units appear (bounded ~2 s).
        let mut units = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(pv) = db.find_pv(name) {
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
    #[cfg(tokio_backend)]
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

        mgr.ensure_subscribed(served.as_bytes(), real.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        // The downstream-facing entry is keyed by the alias the client
        // searched for — this is the FR-2 fix.
        assert!(
            db.find_pv(served).is_some(),
            "shadow PV must be registered under the served (alias) name"
        );
        // The real upstream name is NOT exposed downstream (the client
        // never learns it; only the alias is served).
        assert!(
            db.find_pv(real).is_none(),
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
    #[cfg(tokio_backend)]
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

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        assert!(db.find_pv(name).is_some(), "served == real registers");
        assert!(mgr.is_subscribed(name), "subscription tracked under name");

        mgr.shutdown().await;
    }

    /// BR-3: an upstream death must TEAR THE DOWNSTREAM CHANNEL DOWN, not
    /// post an alarm on a channel the gateway keeps alive.
    ///
    /// C `gatePvData::death` on an active PV does `delete vc; vc = NULL;`
    /// (gatePv.cc:600-601) — the downstream monitor stops and `camonitor`
    /// reports the disconnect. Removing the shadow PV is that deletion here:
    /// it destroys the PV, which retires every downstream channel with
    /// `CA_PROTO_SERVER_DISCONN`, and it makes the name answer
    /// does-not-exist while upstream is away (C `pverDoesNotExistHere`,
    /// gatePv.cc:622). The alarm post this replaced left the channel
    /// connected, so a client latched the last value at a fresh timestamp
    /// and never saw the disconnect at all.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br3_upstream_death_destroys_the_shadow_pv_and_reconnect_respawns_it() {
        let name = "BR3:death";

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        // The upstream IOC's own database, so the test can retire the PV the
        // gateway is subscribed to. Aborting `run()` would NOT do it: `run`
        // spawns its accept loop on a plain `tokio::task::JoinHandle`, which
        // is not abort-on-drop, so the listener and its connections outlive
        // the aborted future.
        let upstream_db = server.database().clone();
        let _upstream = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("upstream connects");
        let shadow = db.find_pv(name).expect("shadow PV registered");
        assert!(!shadow.is_destroyed(), "live upstream, live shadow PV");

        // Retire the PV upstream. The gateway's own CA channel dies exactly
        // as it would if the IOC went away — the same `CA_PROTO_SERVER_DISCONN`
        // this row makes the gateway send downstream.
        upstream_db
            .remove_simple_pv(name)
            .await
            .expect("upstream PV was registered");

        // Wait on the observable, not on a fixed sleep: the shadow PV is
        // gone AND the object the downstream channels hold is destroyed.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if db.find_pv(name).is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "upstream death must remove the shadow PV"
            );
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            shadow.is_destroyed(),
            "the PV object downstream channels still hold must be destroyed, \
             which is what retires them with CA_PROTO_SERVER_DISCONN"
        );
        assert!(
            shadow.subscriber_count() == 0,
            "the monitor stops rather than receiving one more frame"
        );

        // Upstream returns. C rebuilds the virtual channel on the next
        // exist-test after `life()`; here the channel-event owner respawns it
        // from the retired one on reconnect, so the replacement keeps this
        // subscription's write hook instead of coming back unwritable.
        upstream_db
            .add_pv(name, EpicsValue::Double(7.0))
            .await
            .expect("upstream PV comes back");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let revived = loop {
            if let Some(pv) = db.find_pv(name) {
                break pv;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "upstream reconnect must re-register the shadow PV"
            );
            epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        };
        assert!(!revived.is_destroyed(), "the replacement is a live object");
        assert!(
            !Arc::ptr_eq(&revived, &shadow),
            "the destroyed PV is never revived — the retired channels cannot \
             come back to life on it"
        );
        assert!(
            revived.write_hook().is_some(),
            "the replacement carries the subscription's write hook, so a \
             downstream caput still forwards upstream"
        );

        mgr.shutdown().await;
    }

    /// No-cache: death and revival come from the channel's connection
    /// events, as C `connectCB`, so they happen with no downstream monitor —
    /// `release_monitor` once took the only task that could revive the PV.
    /// The revival seeds the replacement from the control get, which is the
    /// only source of 7.0 with no monitor armed.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br3_no_cache_dies_and_revives_with_no_downstream_monitor() {
        let name = "BR3:nc";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let upstream_db = server.database().clone();
        let _upstream = tokio::spawn(async move { server.run().await });
        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let mgr = pinned_manager_full(
            db.clone(),
            cache.clone(),
            CacheMode::NoCache,
            NO_CONNECT_DEADLINE,
        )
        .await;
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("connects");
        let upstream = upstream_db.find_pv(name).expect("upstream PV");

        // With a monitor that is released while the PV is dead.
        mgr.ensure_monitor(name.as_bytes());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while upstream.subscriber_count() == 0 {
            assert!(std::time::Instant::now() < deadline, "value monitor");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        let wait_for = |live: bool, what: &'static str| {
            let db = db.clone();
            async move {
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                while db.find_pv(name).is_some() != live {
                    assert!(std::time::Instant::now() < deadline, "{what}");
                    epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        upstream_db
            .remove_simple_pv(name)
            .await
            .expect("upstream PV");
        wait_for(false, "death with a monitor").await;
        mgr.release_monitor(name.as_bytes());
        upstream_db
            .add_pv(name, EpicsValue::Double(7.0))
            .await
            .expect("upstream back");
        wait_for(true, "revival after the monitor left").await;

        // With no monitor at all.
        upstream_db
            .remove_simple_pv(name)
            .await
            .expect("upstream PV");
        wait_for(false, "death with no monitor").await;
        upstream_db
            .add_pv(name, EpicsValue::Double(7.0))
            .await
            .expect("upstream back");
        wait_for(true, "revival with no monitor").await;
        let revived = db.find_pv(name).expect("revived");
        assert_eq!(
            revived.snapshot().value,
            EpicsValue::Double(7.0),
            "the replacement is seeded by the control get"
        );

        mgr.shutdown().await;
    }

    /// C `gatePvData::death` clears both monitors (gatePv.cc:658-661) and
    /// `getCB` re-arms them only after `life()` (gatePv.cc:1702-1705). A
    /// monitor kept across the death is re-installed with the channel, so
    /// it is on the wire while the revival is held. Here the property task
    /// is held past the first life, so it starts into a dead one.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br3_the_property_monitor_dies_with_the_pv_and_rearms_after_revival() {
        property_monitor_follows_the_life(CacheMode::Cached, true).await;
    }

    /// The same with the property monitor on the wire in the first life, so
    /// the death itself must drop it.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br3_a_live_property_monitor_dies_with_the_pv() {
        property_monitor_follows_the_life(CacheMode::Cached, false).await;
    }

    /// No-cache, a downstream property monitor and no value task: nothing
    /// else follows the life, so the property monitor must drop by itself.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn br3_no_cache_a_lone_property_monitor_dies_with_the_pv() {
        property_monitor_follows_the_life(CacheMode::NoCache, false).await;
    }

    #[cfg(tokio_backend)]
    async fn property_monitor_follows_the_life(mode: CacheMode, start_late: bool) {
        use epics_base_rs::server::snapshot::{DisplayInfo, Snapshot};

        let name = "BR3:prop";
        let units_of = |units: &str| {
            let mut ctrl = Snapshot::new(
                EpicsValue::Double(1.0),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            );
            ctrl.display = Some(DisplayInfo {
                units: units.into(),
                ..Default::default()
            });
            ctrl
        };
        let shadow_units = |db: Arc<PvDatabase>| async move {
            db.find_pv(name)
                .and_then(|pv| pv.snapshot().display)
                .map(|d| d.units.to_string())
        };

        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let upstream_db = server.database().clone();
        let _upstream = tokio::spawn(async move { server.run().await });
        upstream_db
            .set_pv_metadata(name, &units_of("mm"))
            .await
            .expect("upstream metadata");

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let mgr = pinned_manager_full(db.clone(), cache.clone(), mode, NO_CONNECT_DEADLINE).await;
        // Upstream monitors while alive: no-cache arms only the property one.
        let armed = if mode.is_no_cache() { 1 } else { 2 };
        let release_prop = start_late.then(|| {
            let (release_prop, release) = epics_base_rs::runtime::sync::oneshot::channel();
            *mgr.prop_gate.lock() = Some(release);
            release_prop
        });
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("upstream connects");
        assert_eq!(shadow_units(db.clone()).await.as_deref(), Some("mm"));
        mgr.ensure_prop_monitor(name.as_bytes());
        if !start_late {
            let first = upstream_db.find_pv(name).expect("upstream PV");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while first.subscriber_count() < armed {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the monitors on the wire in the first life"
                );
                epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
            }
        }

        upstream_db
            .remove_simple_pv(name)
            .await
            .expect("upstream PV");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while db.find_pv(name).is_some() {
            assert!(std::time::Instant::now() < deadline, "upstream death");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        if let Some(release_prop) = release_prop {
            let _ = release_prop.send(());
        }

        // The revival re-registers through the cache, so holding it keeps
        // the gateway dead while the upstream comes back with new metadata.
        let hold = cache.write().await;
        upstream_db
            .add_pv(name, EpicsValue::Double(7.0))
            .await
            .expect("upstream PV comes back");
        upstream_db
            .set_pv_metadata(name, &units_of("cm"))
            .await
            .expect("upstream metadata");
        let returned = upstream_db.find_pv(name).expect("upstream PV");
        let channel = mgr.subs.lock()[name.as_bytes()].channel.clone();
        channel
            .wait_connected(Duration::from_secs(30))
            .await
            .expect("the channel reconnects");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            returned.subscriber_count(),
            0,
            "no monitor is on the wire before the revival"
        );
        drop(hold);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while shadow_units(db.clone()).await.as_deref() != Some("cm") {
            assert!(
                std::time::Instant::now() < deadline,
                "the revived PV takes the returning upstream's metadata, got {:?}",
                shadow_units(db.clone()).await
            );
            epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(returned.subscriber_count(), armed);

        mgr.shutdown().await;
    }

    /// A revival registers its PV pending at the connect, as C `life()`
    /// answers the exist tests then (gatePv.cc:512-518), and a GET on it
    /// waits for the first event (gateVc.cc:1323-1326). A death while it
    /// waits ends that life: the PV and the armed monitor go, the parked GET
    /// is dropped, and the late first event adds nothing — C `death` deletes
    /// the vc and unmonitors (gatePv.cc:599-601, 656-658). The next life is
    /// readied by its own event.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_death_during_a_revival_ends_that_life() {
        let bound = Duration::from_secs(10);
        let name = "BR3:revive-death";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let upstream_pv = server.database().find_pv(name).expect("upstream PV");
        let _upstream = tokio::spawn(async move { server.run().await });
        let monitors = |want: usize, what: &'static str| {
            let pv = upstream_pv.clone();
            async move {
                let deadline = std::time::Instant::now() + bound;
                while pv.subscriber_count() != want {
                    assert!(std::time::Instant::now() < deadline, "{what}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let mgr = pinned_manager_full(
            db.clone(),
            cache.clone(),
            CacheMode::Cached,
            NO_CONNECT_DEADLINE,
        )
        .await;
        let channel = Arc::new(mgr.client.create_channel(name));
        channel
            .wait_connected(bound)
            .await
            .expect("upstream connects");
        let entry = cache.write().await.get_or_create(name);
        db.add_pv(name, EpicsValue::Double(1.0))
            .await
            .expect("the first life's PV");

        // The test plays the connection events, and stands between the value
        // task and the owner so it decides when a first event arrives.
        let (life_tx, mut life) = tokio::sync::watch::channel(ShadowLife::Alive(0));
        let (events, events_rx) = broadcast::channel(8);
        let (to_owner, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let (from_task, mut first_events) = tokio::sync::mpsc::unbounded_channel();
        let owner = tokio::spawn(
            ChannelEventOwner {
                channel: Arc::downgrade(&channel),
                name: RecordName::from(name),
                cache: cache.clone(),
                db: db.clone(),
                life: life_tx,
                retired: None,
                notifier: Arc::new(ArcSwapOption::empty()),
                beacon_anomaly: mgr.write_env.beacon_anomaly.clone(),
                ctrl_timeout: Duration::ZERO,
                shown: AccessRights {
                    read: true,
                    write: true,
                },
                cached: true,
            }
            .run(events_rx, owner_rx),
        );
        let _task = mgr.spawn_forward_task(
            name.as_bytes(),
            channel.clone(),
            None,
            life.clone(),
            from_task,
        );
        monitors(1, "the first life's monitor").await;

        let mut reach = async |want: ShadowLife| {
            tokio::time::timeout(bound, life.wait_for(|l| *l == want))
                .await
                .unwrap_or_else(|_| panic!("never reached {want:?}"))
                .expect("the owner runs");
        };
        let parked_get = |pv: Arc<epics_base_rs::server::pv::ProcessVariable>| {
            tokio::spawn(async move { pv.read_snapshot().await })
        };
        events.send(ConnectionEvent::Disconnected).unwrap();
        reach(ShadowLife::Dead(0)).await;
        monitors(0, "the death unmonitors").await;

        events.send(ConnectionEvent::Connected).unwrap();
        reach(ShadowLife::Pending(1)).await;
        let pv = db.find_pv(name).expect("registered at the connect");
        assert!(pv.is_pending(), "nothing readied it yet");
        assert!(entry.read().await.state.is_existent());
        let get = parked_get(pv);
        let late = tokio::time::timeout(bound, first_events.recv())
            .await
            .expect("the armed monitor's first event")
            .expect("value task runs");
        assert_eq!(late.life, 1);
        events.send(ConnectionEvent::Disconnected).unwrap();
        reach(ShadowLife::Dead(1)).await;
        monitors(0, "a death while pending drops the armed monitor").await;
        assert!(db.find_pv(name).is_none(), "the death removes a pending PV");
        assert!(
            tokio::time::timeout(bound, get)
                .await
                .expect("the parked GET ends")
                .expect("join")
                .is_err(),
            "a GET parked on the dead life is not answered"
        );
        assert_eq!(entry.read().await.state, PvState::Disconnect);

        events.send(ConnectionEvent::Connected).unwrap();
        reach(ShadowLife::Pending(2)).await;
        let pv = db
            .find_pv(name)
            .expect("the next life registers at its connect");
        let get = parked_get(pv.clone());
        let mut next = tokio::time::timeout(bound, first_events.recv())
            .await
            .expect("the next life's first event")
            .expect("value task runs");
        assert_eq!(next.life, 2);
        // One queue: the ended life's event is handled first, and readying
        // with it would serve 1.0 and swallow this one.
        next.snapshot.value = EpicsValue::Double(9.0);
        to_owner.send(late).unwrap();
        to_owner.send(next).unwrap();
        reach(ShadowLife::Alive(2)).await;
        assert!(!pv.is_pending());
        assert_eq!(pv.snapshot().value, EpicsValue::Double(9.0));
        let got = tokio::time::timeout(bound, get)
            .await
            .expect("the parked GET is answered")
            .expect("join")
            .expect("with the first event");
        assert_eq!(got.value, EpicsValue::Double(9.0));
        monitors(1, "the next life's monitor").await;

        owner.abort();
        mgr.shutdown().await;
    }

    /// The owner readies a pending life with its first event, hands back an
    /// event of that life that a sibling monitor's beat in, to be posted as C
    /// `vcPostEvent`s once the vc is added (gatePv.cc:1432-1441), and drops
    /// one of a life that is over.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn the_owner_readies_a_pending_life_once() {
        let name = "OWN:first";
        let db = Arc::new(PvDatabase::new());
        db.add_simple_pv(
            epics_base_rs::server::pv::ProcessVariable::new(name, EpicsValue::Double(0.0))
                .pending(),
        )
        .await
        .expect("registered");
        let (life_tx, life) = tokio::sync::watch::channel(ShadowLife::Pending(3));
        let mut owner = ChannelEventOwner {
            channel: std::sync::Weak::new(),
            name: RecordName::from(name),
            cache: Arc::new(RwLock::new(PvCache::new())),
            db: db.clone(),
            life: life_tx,
            retired: None,
            notifier: Arc::new(ArcSwapOption::empty()),
            beacon_anomaly: Arc::new(super::super::beacon::BeaconAnomaly::new()),
            ctrl_timeout: Duration::ZERO,
            shown: AccessRights {
                read: true,
                write: true,
            },
            cached: true,
        };
        let mut offer = |life: u64, value: f64| {
            let (returned, mut back) = epics_base_rs::runtime::sync::oneshot::channel();
            owner.first_event(FirstEvent {
                life,
                snapshot: Snapshot::new(
                    EpicsValue::Double(value),
                    0,
                    0,
                    std::time::SystemTime::UNIX_EPOCH,
                ),
                property: false,
                returned,
            });
            back.try_recv().expect("answered at once")
        };

        assert!(offer(2, 1.0).is_none(), "an ended life's event is dropped");
        let pv = db.find_pv(name).expect("registered");
        assert!(pv.is_pending());
        assert!(offer(3, 2.0).is_none(), "the first event is taken");
        assert_eq!(*life.borrow(), ShadowLife::Alive(3));
        assert!(!pv.is_pending());
        assert_eq!(pv.snapshot().value, EpicsValue::Double(2.0));
        assert_eq!(
            offer(3, 4.0).map(|s| s.value),
            Some(EpicsValue::Double(4.0)),
            "a raced event comes back to post"
        );
        assert_eq!(pv.snapshot().value, EpicsValue::Double(2.0));
    }

    /// A pending revival's property event readies it as well, as C
    /// `propEventCB` `vcAdd`s on a revival's initial event (gatePv.cc:1580-
    /// 1602), with the control get's metadata in place first. Only the
    /// property task runs, so nothing else can ready the PV.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_property_event_readies_a_pending_revival() {
        use epics_base_rs::server::snapshot::DisplayInfo;

        let bound = Duration::from_secs(10);
        let name = "BR3:pending-prop";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(4.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let upstream = server.database().clone();
        let _upstream = tokio::spawn(async move { server.run().await });
        let mut ctrl = Snapshot::new(
            EpicsValue::Double(4.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        ctrl.display = Some(DisplayInfo {
            units: "cm".into(),
            ..Default::default()
        });
        upstream
            .set_pv_metadata(name, &ctrl)
            .await
            .expect("upstream metadata");

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let mgr = pinned_manager_full(
            db.clone(),
            cache.clone(),
            CacheMode::Cached,
            NO_CONNECT_DEADLINE,
        )
        .await;
        let channel = Arc::new(mgr.client.create_channel(name));
        channel
            .wait_connected(bound)
            .await
            .expect("upstream connects");
        cache.write().await.get_or_create(name);
        db.add_pv(name, EpicsValue::Double(1.0))
            .await
            .expect("the first life's PV");

        let (life_tx, mut life) = tokio::sync::watch::channel(ShadowLife::Alive(0));
        let (events, events_rx) = broadcast::channel(8);
        let (to_owner, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let owner = tokio::spawn(
            ChannelEventOwner {
                channel: Arc::downgrade(&channel),
                name: RecordName::from(name),
                cache: cache.clone(),
                db: db.clone(),
                life: life_tx,
                retired: None,
                notifier: Arc::new(ArcSwapOption::empty()),
                beacon_anomaly: mgr.write_env.beacon_anomaly.clone(),
                ctrl_timeout: Duration::ZERO,
                shown: AccessRights {
                    read: true,
                    write: true,
                },
                cached: true,
            }
            .run(events_rx, owner_rx),
        );
        let _prop = mgr.spawn_prop_forward_task(
            name.as_bytes(),
            channel.clone(),
            None,
            true,
            life.clone(),
            to_owner,
        );

        events.send(ConnectionEvent::Disconnected).unwrap();
        events.send(ConnectionEvent::Connected).unwrap();
        tokio::time::timeout(bound, life.wait_for(|l| *l == ShadowLife::Alive(1)))
            .await
            .expect("the property event readies the revival")
            .expect("the owner runs");
        let pv = db.find_pv(name).expect("registered");
        assert!(!pv.is_pending());
        assert_eq!(pv.snapshot().value, EpicsValue::Double(4.0));
        assert_eq!(
            pv.metadata().display.map(|d| d.units.to_string()),
            Some("cm".to_string())
        );

        owner.abort();
        mgr.shutdown().await;
    }

    /// Cached life 0 is registered at the connect and its name exists then,
    /// whether or not the upstream ever sends an event: with no read access
    /// C's `monitor()` arms nothing (gatePv.cc:771), yet `pvExistTest` answers
    /// exists for the connected PV (gateServer.cc:1618-1633), and the
    /// downstream read right follows upstream's.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn an_unreadable_upstream_is_visible_at_the_connect() {
        let name = "L0:unreadable";
        let dir = tempfile::tempdir().expect("temp");
        let acf = dir.path().join("none.acf");
        std::fs::write(
            &acf,
            "UAG(nobody) {nobody}\nASG(DEFAULT) {\n RULE(1, READ) {\n  UAG(nobody)\n }\n}\n",
        )
        .expect("write acf");
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .acf_file(acf.to_str().unwrap())
            .expect("load acf")
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let mgr = pinned_manager_full(
            db.clone(),
            cache.clone(),
            CacheMode::Cached,
            Duration::from_secs(2),
        )
        .await;
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("connected, so subscribed");
        let pv = db.find_pv(name).expect("registered at the connect");
        let entry = cache.read().await.get(name).expect("cache entry");
        assert!(entry.read().await.state.is_existent(), "the name exists");
        let hook = pv.access_hook().expect("shadow PV carries the access hook");
        assert!(!hook(b"alice", b"host1").read, "upstream denies read");

        mgr.shutdown().await;
    }

    /// Both upstream monitors are armed together, as C `getCB` calls
    /// `monitor()` then `propMonitor()` (gatePv.cc:1703-1705) — not the
    /// property monitor only once the property task runs. That task is held
    /// throughout, so only a monitor armed with the value one reaches the
    /// upstream.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn life_0_arms_the_property_monitor_with_the_value_monitor() {
        let name = "L0:arm";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(1.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let upstream = server.database().find_pv(name).expect("upstream PV");
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;
        let (release_prop, release) = epics_base_rs::runtime::sync::oneshot::channel();
        *mgr.prop_gate.lock() = Some(release);
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("upstream connects");
        assert!(db.find_pv(name).is_some());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while upstream.subscriber_count() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the property monitor waits for the property task"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = release_prop.send(());
        mgr.shutdown().await;
    }

    /// A GET on the cached life 0 before the value monitor's first event is
    /// answered with that event — its value, alarm and IOC time — not with
    /// the placeholder the PV advertises its native type with (gateVc.cc:
    /// 1323-1326, 1018-1072). The value task is held, so no event lands until
    /// the GET has parked.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn a_get_before_life_0_first_event_waits_for_it() {
        let name = "L0:get";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(6.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let _server = tokio::spawn(async move { server.run().await });

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mgr = pinned_manager(db.clone()).await;
        let (release_value, release) = epics_base_rs::runtime::sync::oneshot::channel();
        *mgr.value_gate.lock() = Some(release);
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("upstream connects");
        let pv = db.find_pv(name).expect("registered at the connect");
        assert!(pv.read_snapshot_local().is_none(), "no value yet");
        let get = tokio::spawn({
            let pv = pv.clone();
            async move { pv.read_snapshot().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!get.is_finished(), "answered before the first event");

        let _ = release_value.send(());
        let got = tokio::time::timeout(Duration::from_secs(10), get)
            .await
            .expect("answered at the first event")
            .expect("join")
            .expect("ok");
        assert_eq!(got.value, EpicsValue::Double(6.0));
        assert_ne!(
            got.timestamp,
            std::time::SystemTime::UNIX_EPOCH.into(),
            "IOC time"
        );

        mgr.shutdown().await;
    }

    /// Life 0's initial property event never readies the PV, even when the
    /// connect-time seed missed and that event seeds the metadata instead: C
    /// ignores it (gatePv.cc:203, 1564-1568), so a GET waits for the value
    /// event rather than taking the control get's value and undefined time.
    // Same in-process `CaServer` gate as the tests above.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread")]
    #[serial(epics_env)]
    async fn life_0_initial_property_event_never_readies_the_pv() {
        use epics_base_rs::server::snapshot::{DisplayInfo, Snapshot};

        let name = "L0:prop";
        let server = CaServer::builder()
            .port(0)
            .pv(name, EpicsValue::Double(6.0))
            .build()
            .await
            .expect("CA server");
        let port = server.udp_port();
        let up_db = server.database().clone();
        let _server = tokio::spawn(async move { server.run().await });
        let mut ctrl = Snapshot::new(
            EpicsValue::Double(6.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        ctrl.display = Some(DisplayInfo {
            units: "mm".into(),
            ..Default::default()
        });
        up_db
            .set_pv_metadata(name, &ctrl)
            .await
            .expect("upstream metadata");

        pin_env(port);
        let db = Arc::new(PvDatabase::new());
        let mut mgr = pinned_manager(db.clone()).await;
        mgr.metadata_seed_timeout = Duration::ZERO;
        let (release_value, release) = epics_base_rs::runtime::sync::oneshot::channel();
        *mgr.value_gate.lock() = Some(release);
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("upstream connects");
        let pv = db.find_pv(name).expect("registered at the connect");

        let seeded = async {
            while pv.snapshot().display.is_none_or(|d| d.units.is_empty()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), seeded)
            .await
            .expect("the initial property event seeds the missed metadata");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(pv.is_pending(), "readied by the initial property event");

        let _ = release_value.send(());
        let got = tokio::time::timeout(Duration::from_secs(10), pv.read_snapshot())
            .await
            .expect("answered at the value event")
            .expect("ok");
        assert_eq!(got.value, EpicsValue::Double(6.0));
        assert_ne!(
            got.timestamp,
            std::time::SystemTime::UNIX_EPOCH.into(),
            "IOC time"
        );

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
    #[cfg(tokio_backend)]
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
        let mgr = pinned_manager_full(
            db.clone(),
            Arc::new(RwLock::new(PvCache::new())),
            CacheMode::Cached,
            Duration::from_secs(1),
        )
        .await;

        let name = "Ghost:pv";
        let result = mgr
            .ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await;

        assert!(
            result.is_err(),
            "ensure_subscribed must fail when the upstream never connects"
        );
        assert!(
            db.find_pv(name).is_none(),
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
    #[cfg(tokio_backend)]
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
            NO_CONNECT_DEADLINE,
        )
        .await;

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        let pv = db.find_pv(name).expect("shadow PV registered");
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
    #[cfg(tokio_backend)]
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

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects to the hosted upstream");

        let pv = db.find_pv(name).expect("shadow PV registered");

        // The cached-mode subscribe above leaves the upstream monitor's first
        // event in flight, and readying the PV writes it into the very cell
        // this test is about to plant SENTINEL in. Land that write first, or
        // the two writers race and the event can overwrite the sentinel in the
        // window between the set and the read below. Upstream never changes
        // value again and the property task writes metadata only, so no later
        // event can reach the value cell.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pv.is_pending() {
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
    #[cfg(tokio_backend)]
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
            NO_CONNECT_DEADLINE,
        )
        .await;

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects");

        // No monitor task until a downstream client subscribes.
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
                .expect("sub tracked")
                .task
                .is_none(),
            "no-cache: no forwarding task before the first downstream monitor"
        );

        // First downstream monitor → task spawned.
        mgr.ensure_monitor(name.as_bytes());
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
                .expect("sub tracked")
                .task
                .is_some(),
            "ensure_monitor must spawn the forwarding task"
        );

        // A second monitor-open is idempotent (task already present).
        mgr.ensure_monitor(name.as_bytes());
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
                .expect("sub tracked")
                .task
                .is_some(),
            "second ensure_monitor is a no-op while a task runs"
        );

        // Last downstream monitor → task aborted.
        mgr.release_monitor(name.as_bytes());
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
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
    #[cfg(tokio_backend)]
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

        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0)
            .await
            .expect("ensure_subscribed connects");

        // Cached: forwarding task exists immediately.
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
                .expect("sub tracked")
                .task
                .is_some(),
            "cached: forwarding task is spawned eagerly at ensure_subscribed"
        );

        // ensure_monitor / release_monitor are no-ops in cached mode.
        mgr.ensure_monitor(name.as_bytes());
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
                .expect("sub tracked")
                .task
                .is_some(),
            "cached: ensure_monitor is a no-op (task stays)"
        );
        mgr.release_monitor(name.as_bytes());
        assert!(
            mgr.subs
                .lock()
                .get(name.as_bytes())
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
    #[cfg(tokio_backend)]
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
            .ensure_subscribed(b"Ghost:cfg", b"Ghost:cfg", None, 0)
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
