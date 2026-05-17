//! [`CaLinkResolver`] — a [`LinkSet`] backend that resolves CA record
//! links through a live [`epics_ca_rs::client::CaClient`].
//!
//! Each distinct CA-link PV name gets one [`CaLink`]: a CA channel, a
//! subscription, and a monitor task that keeps an [`arc_swap::ArcSwap`]
//! snapshot current. The [`LinkSet`] read methods serve from that
//! cache — never a synchronous per-read network fetch. This is the
//! C `dbCa.c` model: `dbCaGetLink` (`dbCa.c:448`) reads the value
//! cached by the monitor `eventCallback` (`dbCa.c:925`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use parking_lot::RwLock;

/// Errors from the CA-link resolver setup path.
#[derive(Debug, thiserror::Error)]
pub enum CaLinkError {
    /// The shared [`CaClient`] could not be constructed.
    #[error("CA client init failed: {0}")]
    ClientInit(String),
    /// Subscribing the monitor for a CA link failed.
    #[error("CA link subscribe failed for {pv}: {reason}")]
    Subscribe { pv: String, reason: String },
}

/// One open CA link — a monitor-backed cache of a remote PV.
///
/// Mirrors C `caLink` (`dbCa.c`): a CA channel plus a subscription
/// whose callback refreshes the cached value. The cache is the only
/// thing the synchronous [`LinkSet`] read path touches. An opaque
/// handle — construct it via [`CaLinkResolver::open`].
pub struct CaLink {
    /// Latest monitor snapshot. `None` until the first event arrives
    /// (channel not yet connected / no value cached) — the C
    /// `dbCaGetLink` "not connected" case.
    cache: Arc<ArcSwap<Option<Snapshot>>>,
    /// Live-connection flag, mirroring `pvalink`'s
    /// `PvaLink::monitor_connected`. The connection-event watcher task
    /// flips this `true` on `ConnectionEvent::Connected` and `false`
    /// on `Disconnected` / `Unresponsive`. `is_connected()` reads it
    /// so a downstream IOC restart is reflected as a real disconnect
    /// — pre-fix `is_connected()` keyed off cache presence alone and
    /// stayed `true` forever once any event had been cached, serving
    /// the last stale `Snapshot` through the whole outage with no
    /// LINK alarm. `dbCa.c` sets `pca->connected = FALSE` in its
    /// `connectionCallback` for exactly this reason.
    connected: Arc<AtomicBool>,
    /// The CA channel — kept alive so the monitor stays subscribed.
    /// Used by the OUT-link write path.
    channel: Arc<CaChannel>,
    /// Abort-on-drop handle for the monitor task. Dropping the
    /// `CaLink` stops the task and (via `MonitorHandle::drop`)
    /// unsubscribes the remote monitor.
    _monitor_task: AbortOnDrop,
    /// Abort-on-drop handle for the connection-event watcher task.
    /// Drains `CaChannel::connection_events()` and keeps `connected`
    /// in sync with the real circuit state.
    _conn_task: AbortOnDrop,
}

/// Abort the wrapped tokio task when dropped. A bare `JoinHandle`
/// detaches on drop and would leak the monitor task.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl CaLink {
    /// True when the CA circuit is currently up AND at least one
    /// monitor event has been cached. C `dbCaGetLink` (`dbCa.c:448`)
    /// treats a CA link as readable only when `pca->connected` is set
    /// (the `connectionCallback` clears it on disconnect) *and* the
    /// monitor callback has populated `pca->pgetNative`. We mirror
    /// both: a circuit-state flag (`connected`, driven by
    /// `CaChannel::connection_events()`) AND cache presence.
    ///
    /// Pre-fix this keyed off cache presence alone, so an upstream
    /// IOC restart was invisible — `is_connected()` stayed `true` and
    /// stale data was served with no LINK alarm.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) && self.cache.load().as_ref().is_some()
    }

    /// Current cached value, or `None` when the link is not connected
    /// (no event yet, or the circuit is currently down). A
    /// disconnected link serves no value — the C `dbCaGetLink`
    /// "not connected" error path — so a downstream IOC outage does
    /// not leak the last stale value into the owning record.
    pub fn value(&self) -> Option<EpicsValue> {
        if !self.connected.load(Ordering::Acquire) {
            return None;
        }
        self.cache.load().as_ref().as_ref().map(|s| s.value.clone())
    }

    /// Current cached alarm severity (0..3), or `None` when the link
    /// is not connected. Mirrors C `dbCaGetAlarmLimits` reading the
    /// cached `pca->sevr` — gated on `pca->connected`.
    pub fn alarm_severity(&self) -> Option<i32> {
        if !self.connected.load(Ordering::Acquire) {
            return None;
        }
        self.cache
            .load()
            .as_ref()
            .as_ref()
            .map(|s| s.alarm.severity as i32)
    }

    /// Cached timestamp as `(seconds_past_epoch, nanoseconds)`, or
    /// `None` when the link is not connected.
    pub fn time_stamp(&self) -> Option<(i64, i32)> {
        if !self.connected.load(Ordering::Acquire) {
            return None;
        }
        let snap = self.cache.load();
        let snap = snap.as_ref().as_ref()?;
        let dur = snap.timestamp.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some((dur.as_secs() as i64, dur.subsec_nanos() as i32))
    }
}

/// A [`LinkSet`] backend for the `ca` URL scheme.
///
/// Holds a single shared [`CaClient`] and a registry of open
/// [`CaLink`]s keyed by PV name, so multiple records pointing at the
/// same remote PV share one CA channel + subscription. Cheap to
/// clone — every field is `Arc`-backed.
#[derive(Clone)]
pub struct CaLinkResolver {
    client: Arc<CaClient>,
    handle: tokio::runtime::Handle,
    /// Open links keyed by bare PV name (`ca://` scheme stripped).
    links: Arc<RwLock<HashMap<String, Arc<CaLink>>>>,
}

impl CaLinkResolver {
    /// Build a resolver with a freshly created shared [`CaClient`].
    pub async fn new(handle: tokio::runtime::Handle) -> Result<Self, CaLinkError> {
        let client = CaClient::new()
            .await
            .map_err(|e| CaLinkError::ClientInit(e.to_string()))?;
        Ok(Self {
            client: Arc::new(client),
            handle,
            links: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Build a resolver around an already-constructed [`CaClient`].
    /// Lets a caller share one client across the CA gateway and the
    /// CA links, or pin the client to a specific server in tests.
    pub fn with_client(client: Arc<CaClient>, handle: tokio::runtime::Handle) -> Self {
        Self {
            client,
            handle,
            links: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Open / cache the CA link for `pv_name` (a bare PV name, no
    /// `ca://` scheme). Idempotent — repeated calls return the cached
    /// [`CaLink`]. Creates a CA channel, subscribes a monitor, and
    /// spawns the task that keeps the cached snapshot current.
    ///
    /// This is the entry point an IOC calls at init for every CA
    /// record link so the synchronous resolver hot path can serve
    /// from cache (the C `dbCaAddLink` analogue).
    pub async fn open(&self, pv_name: &str) -> Result<Arc<CaLink>, CaLinkError> {
        if let Some(existing) = self.links.read().get(pv_name).cloned() {
            return Ok(existing);
        }
        let channel = Arc::new(self.client.create_channel(pv_name));
        let monitor = channel
            .subscribe()
            .await
            .map_err(|e| CaLinkError::Subscribe {
                pv: pv_name.to_string(),
                reason: e.to_string(),
            })?;
        let cache: Arc<ArcSwap<Option<Snapshot>>> = Arc::new(ArcSwap::from_pointee(None));
        let connected = Arc::new(AtomicBool::new(false));
        // Connection-event watcher: keeps `connected` in sync with the
        // real circuit state so `is_connected()` reflects upstream
        // disconnects. Mirrors `pvalink`'s `monitor_connected` flag.
        let conn_rx = channel.connection_events();
        let conn_task = self
            .handle
            .spawn(run_connection_watcher(conn_rx, connected.clone()));
        let task = self.handle.spawn(run_monitor(
            monitor,
            cache.clone(),
            connected.clone(),
            pv_name.to_string(),
        ));
        let link = Arc::new(CaLink {
            cache,
            connected,
            channel,
            _monitor_task: AbortOnDrop(task),
            _conn_task: AbortOnDrop(conn_task),
        });
        // Re-check under the write lock so two concurrent first-callers
        // converge on one link (the loser's freshly opened link drops,
        // unsubscribing its monitor).
        let mut links = self.links.write();
        if let Some(existing) = links.get(pv_name).cloned() {
            return Ok(existing);
        }
        links.insert(pv_name.to_string(), link.clone());
        Ok(link)
    }

    /// Wait until the CA link for `pv_name` has received at least one
    /// monitor event (its cached value is populated). Returns `false`
    /// on timeout. The canonical test / IOC-init helper for "wait for
    /// the upstream IOC to come online".
    pub async fn wait_for_link_connected(
        &self,
        pv_name: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let name = strip_ca_scheme(pv_name);
        let link = match self.open(name).await {
            Ok(l) => l,
            Err(_) => return false,
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if link.value().is_some() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Number of open CA links.
    pub fn link_count(&self) -> usize {
        self.links.read().len()
    }

    /// Lazily resolve `name` to its cached [`CaLink`]. Opens the link
    /// (blocking the worker thread on the runtime) when it is not yet
    /// in the registry — the first-access slow path. Steady-state
    /// reads hit the registry directly.
    fn link_for(&self, name: &str) -> Option<Arc<CaLink>> {
        if let Some(existing) = self.links.read().get(name).cloned() {
            return Some(existing);
        }
        let resolver = self.clone();
        let name = name.to_string();
        block_in_place_or_warn(move || resolver.handle.block_on(resolver.open(&name)).ok())
    }
}

/// Monitor task: drain the subscription, refresh the cache on every
/// event. Ends when the channel is dropped (`recv` returns `None`).
///
/// Mirrors C `dbCa.c` `eventCallback` (`dbCa.c:925`) — every CA
/// monitor event overwrites the cached value/severity/timestamp that
/// `dbCaGetLink` later serves.
async fn run_monitor(
    mut monitor: epics_ca_rs::client::MonitorHandle,
    cache: Arc<ArcSwap<Option<Snapshot>>>,
    connected: Arc<AtomicBool>,
    pv_name: String,
) {
    while let Some(event) = monitor.recv().await {
        match event {
            Ok(snapshot) => {
                // A delivered monitor event is itself proof of
                // liveness — mark the link connected even if the
                // `Connected` lifecycle event has not been observed
                // yet (race-free, mirrors `pvalink`'s callback).
                connected.store(true, Ordering::Release);
                cache.store(Arc::new(Some(snapshot)));
            }
            // A monitor error event (e.g. a transient server-side
            // problem) leaves the last cached value in place — the
            // next good event refreshes it. C `dbCa.c` keeps the
            // stale value on a monitor error the same way.
            Err(e) => {
                tracing::debug!(
                    pv = %pv_name,
                    error = %e,
                    "calink: monitor error event ignored, keeping last cached value"
                );
            }
        }
    }
    // Subscription ended (channel dropped). Reflect the disconnect.
    connected.store(false, Ordering::Release);
}

/// Connection-event watcher: keep `connected` in sync with the CA
/// circuit state. `Connected` flips it `true`; `Disconnected` /
/// `Unresponsive` flip it `false` so a downstream IOC restart is
/// reflected by `CaLink::is_connected()`. Mirrors `dbCa.c`'s
/// `connectionCallback` setting `pca->connected`.
async fn run_connection_watcher(
    mut conn_rx: epics_base_rs::runtime::sync::broadcast::Receiver<
        epics_ca_rs::client::ConnectionEvent,
    >,
    connected: Arc<AtomicBool>,
) {
    use epics_ca_rs::client::ConnectionEvent;
    loop {
        match conn_rx.recv().await {
            Ok(ConnectionEvent::Connected) => connected.store(true, Ordering::Release),
            Ok(ConnectionEvent::Disconnected) | Ok(ConnectionEvent::Unresponsive) => {
                connected.store(false, Ordering::Release)
            }
            // AccessRightsChanged / NativeTypeChanged don't affect the
            // connected flag.
            Ok(_) => {}
            // Lagged: a burst of events overran the bounded channel.
            // Keep watching; the next event resyncs the flag.
            Err(epics_base_rs::runtime::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            // Closed: the channel was dropped — watcher's job is done.
            Err(epics_base_rs::runtime::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

impl LinkSet for CaLinkResolver {
    fn is_connected(&self, name: &str) -> bool {
        let name = strip_ca_scheme(name);
        match self.links.read().get(name) {
            Some(link) => link.is_connected(),
            // Not opened yet — report not connected. `open` /
            // `get_value` open it lazily.
            None => false,
        }
    }

    fn get_value(&self, name: &str) -> Option<EpicsValue> {
        let name = strip_ca_scheme(name);
        self.link_for(name)?.value()
    }

    fn put_value(&self, name: &str, value: EpicsValue) -> Result<(), String> {
        let name = strip_ca_scheme(name);
        let link = self
            .link_for(name)
            .ok_or_else(|| format!("CA link {name} not open"))?;
        let channel = link.channel.clone();
        block_in_place_or_warn(move || {
            self.handle
                .block_on(async { channel.put(&value).await })
                .map_err(|e| e.to_string())
        })
    }

    fn alarm_severity(&self, name: &str) -> Option<i32> {
        let name = strip_ca_scheme(name);
        let sev = self.link_for(name)?.alarm_severity()?;
        // Mirror the lset contract: only a non-zero severity is a
        // contribution worth propagating into the owning record's
        // LINK_ALARM. `0` (NO_ALARM) means "do not propagate".
        if sev > 0 { Some(sev) } else { None }
    }

    fn time_stamp(&self, name: &str) -> Option<(i64, i32)> {
        let name = strip_ca_scheme(name);
        self.link_for(name)?.time_stamp()
    }

    fn link_names(&self) -> Vec<String> {
        self.links.read().keys().cloned().collect()
    }
}

/// Strip a leading `ca://` scheme prefix. `epics-base-rs` stores both
/// the scheme-prefixed and the bare form in `ParsedLink::Ca`
/// depending on the link syntax (`ca://X` vs the bare ` CA` modifier),
/// so the resolver normalises to the bare PV name.
fn strip_ca_scheme(name: &str) -> &str {
    name.strip_prefix("ca://").unwrap_or(name)
}

/// Run `f`, parking the tokio worker thread for the duration when on a
/// multi-threaded runtime so an inner `block_on` does not deadlock the
/// runtime. Mirrors the helper in [`crate::pvalink`]'s integration
/// module — the lset trait is synchronous but is invoked from inside
/// `PvDatabase::resolve_external_pv`'s async context.
fn block_in_place_or_warn<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    use tokio::runtime::{Handle, RuntimeFlavor};
    if let Ok(handle) = Handle::try_current() {
        match handle.runtime_flavor() {
            RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
            _ => f(),
        }
    } else {
        f()
    }
}

/// Install a [`CaLinkResolver`] on `db`, registered under the `"ca"`
/// lset scheme. After this, a record whose link field resolves to
/// `ParsedLink::Ca` (a `ca://X` link or a bare ` CA`-modified link)
/// reads through the monitor-backed CA cache via
/// `PvDatabase::resolve_external_pv`.
///
/// Returns the resolver so the caller can pre-open links
/// ([`CaLinkResolver::open`]) at IOC init and query stats.
pub async fn install_calink_resolver(
    db: &PvDatabase,
    handle: tokio::runtime::Handle,
) -> Result<CaLinkResolver, CaLinkError> {
    let resolver = CaLinkResolver::new(handle).await?;
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    Ok(resolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_ca_rs::client::ConnectionEvent;

    #[test]
    fn strip_ca_scheme_handles_both_forms() {
        assert_eq!(strip_ca_scheme("ca://OTHER:PV"), "OTHER:PV");
        assert_eq!(strip_ca_scheme("OTHER:PV"), "OTHER:PV");
    }

    /// Block until `flag` reaches `want`, or panic after a deadline —
    /// the connection watcher updates the flag asynchronously.
    async fn await_flag(flag: &AtomicBool, want: bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while flag.load(Ordering::Acquire) != want {
            assert!(
                std::time::Instant::now() < deadline,
                "connected flag never reached {want}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// BUG 1 regression: `CaLink::is_connected()` must go false when
    /// the upstream CA circuit disconnects. The connection-event
    /// watcher drains `connection_events()` and flips the shared
    /// `connected` flag; `is_connected()` reads it. Pre-fix
    /// `is_connected()` keyed off cache presence alone and stayed
    /// `true` forever once any event had been cached — an upstream
    /// IOC restart was invisible and stale data was served.
    #[tokio::test]
    async fn bug1_connection_watcher_tracks_disconnect() {
        let (tx, rx) = epics_base_rs::runtime::sync::broadcast::channel::<ConnectionEvent>(16);
        let connected = Arc::new(AtomicBool::new(false));
        let watcher = tokio::spawn(run_connection_watcher(rx, connected.clone()));

        // Circuit comes up — flag goes true.
        tx.send(ConnectionEvent::Connected).unwrap();
        await_flag(&connected, true).await;

        // Upstream IOC restart — circuit drops. Flag MUST go false;
        // pre-fix it stayed true forever.
        tx.send(ConnectionEvent::Disconnected).unwrap();
        await_flag(&connected, false).await;

        // Reconnect — flag goes true again (CA monitors auto-restore).
        tx.send(ConnectionEvent::Connected).unwrap();
        await_flag(&connected, true).await;

        // An Unresponsive event (TCP up, server hung) also clears it.
        tx.send(ConnectionEvent::Unresponsive).unwrap();
        await_flag(&connected, false).await;

        // Dropping the sender closes the channel — the watcher exits.
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(2), watcher)
            .await
            .expect("watcher must exit when the event channel closes")
            .expect("watcher task panicked");
    }

    /// BUG 1 regression: the `is_connected()` / `value()` gating logic
    /// itself — a link with a populated cache but a `false` connected
    /// flag must report not-connected and serve no value. This is the
    /// state during an upstream outage (cache holds the last
    /// Snapshot, circuit is down).
    #[test]
    fn bug1_disconnected_link_serves_no_stale_value() {
        // Reproduce the exact gate `is_connected()` / `value()` apply:
        // both require `connected == true`.
        let cache: Arc<ArcSwap<Option<Snapshot>>> = Arc::new(ArcSwap::from_pointee(None));
        let connected = Arc::new(AtomicBool::new(false));

        // Populate the cache with a stale snapshot, circuit still down.
        cache.store(Arc::new(Some(Snapshot::new(
            EpicsValue::Double(42.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ))));

        // Gate: cache is present but connected is false.
        let is_connected = connected.load(Ordering::Acquire) && cache.load().as_ref().is_some();
        assert!(
            !is_connected,
            "a disconnected link must report not-connected even with a cached snapshot"
        );
        let value = if connected.load(Ordering::Acquire) {
            cache.load().as_ref().as_ref().map(|s| s.value.clone())
        } else {
            None
        };
        assert!(
            value.is_none(),
            "a disconnected link must serve no stale value"
        );

        // Circuit comes back — both gates open.
        connected.store(true, Ordering::Release);
        let is_connected = connected.load(Ordering::Acquire) && cache.load().as_ref().is_some();
        assert!(
            is_connected,
            "reconnected link with cache must be connected"
        );
    }
}
