//! Gateway statistics.
//!
//! Tracks runtime metrics and exposes them as PVs hosted by the gateway's
//! own shadow [`PvDatabase`]. Downstream clients can read these PVs to
//! monitor the gateway itself (`gateway:totalPvs`, `gateway:vcCount`, etc.).
//!
//! Corresponds to C++ `gateStat`.
//!
//! ## Exposed PVs
//!
//! All names use the configurable prefix (default `"gateway:"`).
//!
//! Native names:
//!
//! | PV | Type | Description |
//! |----|------|-------------|
//! | `<prefix>totalPvs` | Long | Total entries in the cache (all states) |
//! | `<prefix>upstreamCount` | Long | Active upstream subscriptions |
//! | `<prefix>connectingCount` | Long | PVs in Connecting state |
//! | `<prefix>activeCount` | Long | PVs in Active state |
//! | `<prefix>inactiveCount` | Long | PVs in Inactive state |
//! | `<prefix>deadCount` | Long | PVs in Dead state |
//! | `<prefix>eventRate` | Double | Events/sec averaged over stats interval |
//! | `<prefix>totalEvents` | Long | Cumulative event count |
//! | `<prefix>heartbeat` | Long | Incrementing heartbeat counter |
//! | `<prefix>putCount` | Long | Cumulative put count (for putlog) |
//! | `<prefix>readOnlyRejects` | Long | Puts rejected because read_only=true |
//! | `<prefix>perHostConnections` | Long | Distinct downstream client hosts |
//!
//! C++ ca-gateway compatibility aliases (B-G10) — kept so dashboards
//! and scripts written against the C source's `gateServer.cc:1903-1965`
//! names keep working against the Rust gateway:
//!
//! | PV | Type | Maps to |
//! |----|------|---------|
//! | `<prefix>vctotal` | Long | totalPvs (virtual-channel total) |
//! | `<prefix>pvtotal` | Long | totalPvs (real-PV total — same source as vctotal in C) |
//! | `<prefix>connected` | Long | active + inactive (upstream-alive) |
//! | `<prefix>active` | Long | activeCount |
//! | `<prefix>inactive` | Long | inactiveCount |
//! | `<prefix>unconnected` | Long | connecting + dead (upstream-not-alive) |
//! | `<prefix>dead` | Long | deadCount |
//! | `<prefix>connecting` | Long | connectingCount |
//! | `<prefix>disconnected` | Long | deadCount (alias — C source treats these as the same bucket) |
//! | `<prefix>clientEventRate` | Double | eventRate |
//!
//! Not implemented (intentional scope — operational metrics covered by
//! the [`metrics`] crate Prometheus export instead):
//! - `fd` (open-file-descriptor count) — Unix-specific syscalls
//! - RATE_STATS internals (`clientEventCount`, `postEventCount`,
//!   `loopCount`) — scheduler-loop instrumentation tied to the C++
//!   event-driven main loop; the Rust async runtime has no equivalent.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use tokio::sync::{Mutex, RwLock};

use super::cache::PvCache;

/// Gateway runtime statistics.
pub struct Stats {
    prefix: String,
    /// Cumulative event count from upstream (incremented in cache updater).
    pub total_events: AtomicU64,
    /// Cumulative put count.
    pub put_count: AtomicU64,
    /// Puts rejected because gateway is in read-only mode.
    pub read_only_rejects: AtomicU64,
    /// Heartbeat counter.
    pub heartbeat: AtomicU64,
    /// Per-host connection set, kept behind a mutex for distinct counting.
    per_host: Mutex<HashSet<String>>,
    /// Last refresh timestamp for event rate calculation.
    last_refresh: Mutex<Instant>,
    /// Last total_events value at refresh time, for delta calculation.
    last_total_events: AtomicU64,
}

impl Stats {
    pub fn new(prefix: String) -> Self {
        Self {
            prefix,
            total_events: AtomicU64::new(0),
            put_count: AtomicU64::new(0),
            read_only_rejects: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            per_host: Mutex::new(HashSet::new()),
            last_refresh: Mutex::new(Instant::now()),
            last_total_events: AtomicU64::new(0),
        }
    }

    /// Record an upstream event.
    pub fn record_event(&self) {
        self.total_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a put operation.
    pub fn record_put(&self) {
        self.put_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a put that was rejected by read-only mode.
    pub fn record_readonly_reject(&self) {
        self.read_only_rejects.fetch_add(1, Ordering::Relaxed);
    }

    /// Track a downstream client host (for per-host connection count).
    pub async fn record_host(&self, host: &str) {
        self.per_host.lock().await.insert(host.to_string());
    }

    /// Forget a downstream client host (on disconnect).
    pub async fn forget_host(&self, host: &str) {
        self.per_host.lock().await.remove(host);
    }

    /// Distinct downstream client host count.
    pub async fn host_count(&self) -> usize {
        self.per_host.lock().await.len()
    }

    /// Pre-register all stats PVs in the shadow database with placeholder values.
    /// Called once during gateway build.
    pub async fn publish_initial(&self, db: &PvDatabase) {
        let p = &self.prefix;
        if p.is_empty() {
            return;
        }

        for (suffix, init) in [
            ("totalPvs", EpicsValue::Long(0)),
            ("upstreamCount", EpicsValue::Long(0)),
            ("connectingCount", EpicsValue::Long(0)),
            ("activeCount", EpicsValue::Long(0)),
            ("inactiveCount", EpicsValue::Long(0)),
            ("deadCount", EpicsValue::Long(0)),
            ("eventRate", EpicsValue::Double(0.0)),
            ("totalEvents", EpicsValue::Long(0)),
            ("heartbeat", EpicsValue::Long(0)),
            ("putCount", EpicsValue::Long(0)),
            ("readOnlyRejects", EpicsValue::Long(0)),
            ("perHostConnections", EpicsValue::Long(0)),
            // B-G10: aliases matching C++ ca-gateway (gateServer.cc:
            // 1903-1965) so dashboards/scripts written against the C
            // names keep working. Connected = active + inactive
            // (both are "upstream is alive"); pvtotal/vctotal are
            // both alias for totalPvs in the C source.
            ("vctotal", EpicsValue::Long(0)),
            ("pvtotal", EpicsValue::Long(0)),
            ("connected", EpicsValue::Long(0)),
            ("active", EpicsValue::Long(0)),
            ("inactive", EpicsValue::Long(0)),
            ("unconnected", EpicsValue::Long(0)),
            ("dead", EpicsValue::Long(0)),
            ("connecting", EpicsValue::Long(0)),
            ("disconnected", EpicsValue::Long(0)),
            ("clientEventRate", EpicsValue::Double(0.0)),
        ] {
            let pv = format!("{p}{suffix}");
            if let Err(e) = db.add_pv(&pv, init).await {
                tracing::warn!(
                    pv = %pv,
                    error = %e,
                    "ca_gateway stats: pre-register skipped (name already in use)"
                );
            }
        }
    }

    /// Refresh stats PVs in the database from current cache + counters.
    /// Called periodically by the stats timer in the main event loop.
    pub async fn refresh(
        &self,
        cache: &RwLock<PvCache>,
        db: &PvDatabase,
        cache_size: usize,
        upstream_count: usize,
    ) {
        if self.prefix.is_empty() {
            return;
        }

        // Compute counts by state via the single-pass count_states
        // helper (B-G13). Snapshot inside count_states releases the
        // per-entry Arc borrows once collected, so the outer
        // `cache.read().await` doesn't span the per-entry awaits.
        let cache_guard = cache.read().await;
        let (connecting, active, inactive, dead, _disconnect) = cache_guard.count_states().await;
        drop(cache_guard);

        // Compute event rate over the interval since last refresh
        let now = Instant::now();
        let mut last = self.last_refresh.lock().await;
        let elapsed = now.duration_since(*last).as_secs_f64();
        *last = now;
        drop(last);

        let total_events = self.total_events.load(Ordering::Relaxed);
        let last_events = self.last_total_events.swap(total_events, Ordering::Relaxed);
        let delta = total_events.saturating_sub(last_events);
        let event_rate = if elapsed > 0.0 {
            delta as f64 / elapsed
        } else {
            0.0
        };

        let put_count = self.put_count.load(Ordering::Relaxed);
        let readonly = self.read_only_rejects.load(Ordering::Relaxed);
        let heartbeat = self.heartbeat.load(Ordering::Relaxed);
        let host_count = self.host_count().await;

        // Fan all 12 stats PV writes out concurrently. Each
        // `put_pv_and_post` is independent (no shared lock between them
        // beyond the per-PV `RwLock`), so a single `tokio::join!` cuts
        // refresh latency from `12 × put_latency` to `max(put_latency)`.
        let p = &self.prefix;
        // Bind names to locals so the futures inside `join!` borrow them
        // for long enough; bare `&format!(...)` would be dropped at the
        // end of the macro line.
        let n_total = format!("{p}totalPvs");
        let n_upstream = format!("{p}upstreamCount");
        let n_connecting = format!("{p}connectingCount");
        let n_active = format!("{p}activeCount");
        let n_inactive = format!("{p}inactiveCount");
        let n_dead = format!("{p}deadCount");
        let n_rate = format!("{p}eventRate");
        let n_events = format!("{p}totalEvents");
        let n_heartbeat = format!("{p}heartbeat");
        let n_put = format!("{p}putCount");
        let n_readonly = format!("{p}readOnlyRejects");
        let n_hosts = format!("{p}perHostConnections");
        // C++ ca-gateway aliases (B-G10).
        let n_vctotal = format!("{p}vctotal");
        let n_pvtotal = format!("{p}pvtotal");
        let n_connected = format!("{p}connected");
        let n_active_alias = format!("{p}active");
        let n_inactive_alias = format!("{p}inactive");
        let n_unconnected = format!("{p}unconnected");
        let n_dead_alias = format!("{p}dead");
        let n_connecting_alias = format!("{p}connecting");
        let n_disconnected = format!("{p}disconnected");
        let n_client_event_rate = format!("{p}clientEventRate");
        let connected = (active + inactive) as i32;
        let unconnected = (connecting + dead) as i32;
        let _ = tokio::join!(
            db.put_pv_and_post(&n_total, EpicsValue::Long(cache_size as i32)),
            db.put_pv_and_post(&n_upstream, EpicsValue::Long(upstream_count as i32)),
            db.put_pv_and_post(&n_connecting, EpicsValue::Long(connecting as i32)),
            db.put_pv_and_post(&n_active, EpicsValue::Long(active as i32)),
            db.put_pv_and_post(&n_inactive, EpicsValue::Long(inactive as i32)),
            db.put_pv_and_post(&n_dead, EpicsValue::Long(dead as i32)),
            db.put_pv_and_post(&n_rate, EpicsValue::Double(event_rate)),
            db.put_pv_and_post(&n_events, EpicsValue::Long(total_events as i32)),
            db.put_pv_and_post(&n_heartbeat, EpicsValue::Long(heartbeat as i32)),
            db.put_pv_and_post(&n_put, EpicsValue::Long(put_count as i32)),
            db.put_pv_and_post(&n_readonly, EpicsValue::Long(readonly as i32)),
            db.put_pv_and_post(&n_hosts, EpicsValue::Long(host_count as i32)),
            db.put_pv_and_post(&n_vctotal, EpicsValue::Long(cache_size as i32)),
            db.put_pv_and_post(&n_pvtotal, EpicsValue::Long(cache_size as i32)),
            db.put_pv_and_post(&n_connected, EpicsValue::Long(connected)),
            db.put_pv_and_post(&n_active_alias, EpicsValue::Long(active as i32)),
            db.put_pv_and_post(&n_inactive_alias, EpicsValue::Long(inactive as i32)),
            db.put_pv_and_post(&n_unconnected, EpicsValue::Long(unconnected)),
            db.put_pv_and_post(&n_dead_alias, EpicsValue::Long(dead as i32)),
            db.put_pv_and_post(&n_connecting_alias, EpicsValue::Long(connecting as i32)),
            db.put_pv_and_post(&n_disconnected, EpicsValue::Long(dead as i32)),
            db.put_pv_and_post(&n_client_event_rate, EpicsValue::Double(event_rate)),
        );
    }

    /// Increment the heartbeat counter and post to the heartbeat PV.
    pub async fn heartbeat_tick(&self, db: &PvDatabase) {
        let n = self.heartbeat.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.prefix.is_empty() {
            let _ = db
                .put_pv_and_post(
                    &format!("{}heartbeat", self.prefix),
                    EpicsValue::Long(n as i32),
                )
                .await;
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment() {
        let stats = Stats::new("g:".into());
        assert_eq!(stats.total_events.load(Ordering::Relaxed), 0);
        stats.record_event();
        stats.record_event();
        assert_eq!(stats.total_events.load(Ordering::Relaxed), 2);

        stats.record_put();
        assert_eq!(stats.put_count.load(Ordering::Relaxed), 1);

        stats.record_readonly_reject();
        assert_eq!(stats.read_only_rejects.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn host_tracking() {
        let stats = Stats::new("g:".into());
        assert_eq!(stats.host_count().await, 0);

        stats.record_host("host1").await;
        stats.record_host("host2").await;
        stats.record_host("host1").await; // duplicate
        assert_eq!(stats.host_count().await, 2);

        stats.forget_host("host1").await;
        assert_eq!(stats.host_count().await, 1);
    }

    #[tokio::test]
    async fn publish_initial_creates_pvs() {
        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        assert!(db.has_name("g:totalPvs").await);
        assert!(db.has_name("g:heartbeat").await);
        assert!(db.has_name("g:eventRate").await);
    }

    #[tokio::test]
    async fn empty_prefix_skips_publish() {
        let stats = Stats::new("".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;
        assert!(!db.has_name("totalPvs").await);
    }
}
