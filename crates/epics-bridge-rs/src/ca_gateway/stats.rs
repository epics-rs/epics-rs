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
//! All C gateStat PVs are served as DBR_DOUBLE (`gateStat.cc:27`
//! `#define STAT_DOUBLE`), so these C-name aliases are Double even for
//! integer counts, matching the native type a C-compat client sees.
//!
//! | PV | Type | Maps to |
//! |----|------|---------|
//! | `<prefix>vctotal` | Double | total_vc — entries with ≥1 downstream client |
//! | `<prefix>pvtotal` | Double | total_pv — cache size (all real PVs) |
//! | `<prefix>connected` | Double | active + inactive (upstream-alive) |
//! | `<prefix>active` | Double | activeCount |
//! | `<prefix>inactive` | Double | inactiveCount |
//! | `<prefix>unconnected` | Double | connecting + dead + disconnect (statUnconnected) |
//! | `<prefix>dead` | Double | deadCount |
//! | `<prefix>connecting` | Double | connectingCount |
//! | `<prefix>disconnected` | Double | disconnect-state count (statDisconnected) |
//! | `<prefix>clientEventRate` | Double | eventRate |
//! | `<prefix>existTestRate` | Double | pvExistTest (search) resolutions/sec — separate from eventRate |
//!
//! RATE_STATS internals (B5) — tokio-model equivalents of the C++
//! ca-gateway `gateServer` RATE_STATS counters from `gateServer.cc`.
//! The C source increments these inside its event-driven main loop;
//! the Rust port increments atomic counters at the equivalent points
//! (events received from upstream, monitor posts fanned out
//! downstream, run-loop iterations):
//!
//! | PV | Type | Maps to |
//! |----|------|---------|
//! | `<prefix>fd` | Double | Current open file-descriptor count (C statFd) |
//! | `<prefix>clientEventCount` | Long | Cumulative upstream events received (Rust-native, no C name) |
//! | `<prefix>postEventCount` | Long | Cumulative monitor posts fanned downstream |
//! | `<prefix>loopCount` | Long | Cumulative gateway run-loop iterations |

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
    /// B5 RATE_STATS: cumulative monitor posts fanned out to the
    /// downstream shadow database. Incremented once per
    /// `put_pv_and_post` in the upstream forwarding task. Mirrors C++
    /// ca-gateway `gateServer::postEventCount`.
    pub post_event_count: AtomicU64,
    /// B5 RATE_STATS: cumulative gateway run-loop iterations. The C++
    /// gateway increments this per fdManager event-loop pass; the
    /// tokio port has no single event loop, so it is incremented once
    /// per periodic maintenance tick (cleanup / stats / heartbeat
    /// timers each call `record_loop`). Mirrors `gateServer::loopCount`.
    pub loop_count: AtomicU64,
    /// Per-host connection set, kept behind a mutex for distinct counting.
    per_host: Mutex<HashSet<String>>,
    /// Cumulative `pvExistTest` count (downstream search resolutions).
    /// C ca-gateway tracks this separately as `exist_count` and exposes
    /// it as the `existTestRate` stat (`gateServer.cc:1497,1991`); it is
    /// NOT an upstream event and must not feed `total_events` /
    /// `clientEventCount` / `eventRate`.
    pub exist_count: AtomicU64,
    /// Last refresh timestamp for event rate calculation.
    last_refresh: Mutex<Instant>,
    /// Last total_events value at refresh time, for delta calculation.
    last_total_events: AtomicU64,
    /// Last exist_count value at refresh time, for existTestRate delta.
    last_exist_count: AtomicU64,
}

impl Stats {
    pub fn new(prefix: String) -> Self {
        Self {
            prefix,
            total_events: AtomicU64::new(0),
            put_count: AtomicU64::new(0),
            read_only_rejects: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            post_event_count: AtomicU64::new(0),
            loop_count: AtomicU64::new(0),
            exist_count: AtomicU64::new(0),
            per_host: Mutex::new(HashSet::new()),
            last_refresh: Mutex::new(Instant::now()),
            last_total_events: AtomicU64::new(0),
            last_exist_count: AtomicU64::new(0),
        }
    }

    /// Record an upstream event. Only the upstream CA monitor callback
    /// (`upstream.rs`) calls this — a `pvExistTest` search resolution is
    /// NOT an upstream event (use [`Self::record_exist_test`]).
    pub fn record_event(&self) {
        self.total_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a downstream `pvExistTest` (search resolution). C
    /// ca-gateway bumps a separate `exist_count` here
    /// (`gateServer.cc:1497`), feeding the `existTestRate` stat — it does
    /// not touch the upstream event counters.
    pub fn record_exist_test(&self) {
        self.exist_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a put operation.
    pub fn record_put(&self) {
        self.put_count.fetch_add(1, Ordering::Relaxed);
    }

    /// B5: record a monitor post fanned out to the downstream shadow
    /// database. Called once per `put_pv_and_post` in the upstream
    /// forwarding task.
    pub fn record_post_event(&self) {
        self.post_event_count.fetch_add(1, Ordering::Relaxed);
    }

    /// B5: record one gateway run-loop iteration. Called from each
    /// periodic maintenance tick (cleanup / stats / heartbeat).
    pub fn record_loop(&self) {
        self.loop_count.fetch_add(1, Ordering::Relaxed);
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
            // (both are "upstream is alive"). pvtotal = total_pv (cache
            // size); vctotal = total_vc (entries with a downstream
            // client) — these are distinct counters in the C source.
            // C ca-gateway serves every gateStat PV as DBR_DOUBLE
            // (gateStat.cc:27 `#define STAT_DOUBLE` → bestExternalType()
            // = aitEnumFloat64, gateStat.cc:235-238). The C-name compat
            // aliases must therefore be Double, not Long, or a downstream
            // client sees a divergent native type at CREATE_CHANNEL.
            ("vctotal", EpicsValue::Double(0.0)),
            ("pvtotal", EpicsValue::Double(0.0)),
            ("connected", EpicsValue::Double(0.0)),
            ("active", EpicsValue::Double(0.0)),
            ("inactive", EpicsValue::Double(0.0)),
            ("unconnected", EpicsValue::Double(0.0)),
            ("dead", EpicsValue::Double(0.0)),
            ("connecting", EpicsValue::Double(0.0)),
            ("disconnected", EpicsValue::Double(0.0)),
            ("clientEventRate", EpicsValue::Double(0.0)),
            // Downstream search-resolution rate. C ca-gateway exposes
            // `existTestRate` (gateServer.cc:1991) from a counter
            // separate from the upstream event counters.
            ("existTestRate", EpicsValue::Double(0.0)),
            // B5: RATE_STATS internals — tokio-model equivalents of
            // the C++ ca-gateway gateServer counters. `fd` is a C
            // gateStat PV (statFd) → DBR_DOUBLE like the other stats;
            // clientEventCount/postEventCount/loopCount have no C-name
            // counterpart (C exposes rates, not raw counts), so they
            // stay Long as a Rust-native API surface.
            ("fd", EpicsValue::Double(0.0)),
            ("clientEventCount", EpicsValue::Long(0)),
            ("postEventCount", EpicsValue::Long(0)),
            ("loopCount", EpicsValue::Long(0)),
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
        let (connecting, active, inactive, dead, disconnect, vc) = cache_guard.count_states().await;
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

        // Exist-test rate from its own counter (C `existTestRate`,
        // gateServer.cc:1991) — independent of the upstream event rate.
        let exist_count = self.exist_count.load(Ordering::Relaxed);
        let last_exist = self.last_exist_count.swap(exist_count, Ordering::Relaxed);
        let exist_delta = exist_count.saturating_sub(last_exist);
        let exist_test_rate = if elapsed > 0.0 {
            exist_delta as f64 / elapsed
        } else {
            0.0
        };

        let put_count = self.put_count.load(Ordering::Relaxed);
        let readonly = self.read_only_rejects.load(Ordering::Relaxed);
        let heartbeat = self.heartbeat.load(Ordering::Relaxed);
        let host_count = self.host_count().await;

        // B5 RATE_STATS internals.
        let post_event_count = self.post_event_count.load(Ordering::Relaxed);
        let loop_count = self.loop_count.load(Ordering::Relaxed);
        // `clientEventCount` is the same upstream-event source as
        // `total_events` — the C++ gateway exposes both the rate PV
        // (`clientEventRate`) and the raw count (`clientEventCount`)
        // from one counter.
        let client_event_count = total_events;

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
        let n_exist_test_rate = format!("{p}existTestRate");
        // B5 RATE_STATS PV names.
        let n_fd = format!("{p}fd");
        let n_client_event_count = format!("{p}clientEventCount");
        let n_post_event_count = format!("{p}postEventCount");
        let n_loop_count = format!("{p}loopCount");
        // C `connected` = statAlive = active + inactive (gatePv.cc bumps
        // total_alive only for those two states). `unconnected` =
        // statUnconnected, which C increments for Connecting, Dead AND
        // Disconnect (gatePv.cc:315,328,608,617) — the Disconnect state
        // must be counted here, not dropped.
        let connected = (active + inactive) as i32;
        let unconnected = (connecting + dead + disconnect) as i32;
        // Sample the live open-fd count. `open_fd_count` reads a kernel
        // directory; on the rare platform where neither exists, keep
        // the PV at its previous value rather than posting a bogus 0.
        let fd_count = open_fd_count();
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
            // C-name compat aliases are served as DBR_DOUBLE to match C
            // ca-gateway (gateStat STAT_DOUBLE). `vctotal` counts only
            // entries with an attached downstream client (C `total_vc`,
            // gateVc.cc:406,472), distinct from the `pvtotal` cache size
            // (C `total_pv`, gatePv.cc:183).
            db.put_pv_and_post(&n_vctotal, EpicsValue::Double(vc as f64)),
            db.put_pv_and_post(&n_pvtotal, EpicsValue::Double(cache_size as f64)),
            db.put_pv_and_post(&n_connected, EpicsValue::Double(connected as f64)),
            db.put_pv_and_post(&n_active_alias, EpicsValue::Double(active as f64)),
            db.put_pv_and_post(&n_inactive_alias, EpicsValue::Double(inactive as f64)),
            db.put_pv_and_post(&n_unconnected, EpicsValue::Double(unconnected as f64)),
            db.put_pv_and_post(&n_dead_alias, EpicsValue::Double(dead as f64)),
            db.put_pv_and_post(&n_connecting_alias, EpicsValue::Double(connecting as f64)),
            // C `disconnected` = statDisconnected = the Disconnect-state
            // count (gatePv.cc:607,616), NOT the Dead count.
            db.put_pv_and_post(&n_disconnected, EpicsValue::Double(disconnect as f64)),
            db.put_pv_and_post(&n_client_event_rate, EpicsValue::Double(event_rate)),
            db.put_pv_and_post(&n_exist_test_rate, EpicsValue::Double(exist_test_rate)),
            // B5 RATE_STATS internals.
            db.put_pv_and_post(
                &n_client_event_count,
                EpicsValue::Long(client_event_count as i32),
            ),
            db.put_pv_and_post(
                &n_post_event_count,
                EpicsValue::Long(post_event_count as i32),
            ),
            db.put_pv_and_post(&n_loop_count, EpicsValue::Long(loop_count as i32)),
        );

        // `fd` is posted separately because it is only available when
        // the kernel fd directory could be read; on an unsupported
        // platform we leave the PV at its last value rather than
        // posting a misleading 0.
        if let Some(fd) = fd_count {
            // C `fd` (statFd) is a gateStat PV → DBR_DOUBLE.
            let _ = db
                .put_pv_and_post(&n_fd, EpicsValue::Double(fd as f64))
                .await;
        }
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

/// B5: count the process's currently open file descriptors.
///
/// The C++ ca-gateway publishes `fd` from `fdManager`'s registered
/// descriptor table. The tokio port has no such table, so the count
/// is derived from the kernel's per-process fd directory:
///
/// - Linux: `/proc/self/fd`
/// - macOS / *BSD: `/dev/fd`
///
/// Both directories list one entry per open descriptor. The reader
/// handle that `read_dir` itself opens is subtracted so the reported
/// count reflects the steady-state fd usage rather than transiently
/// counting the enumeration handle. Returns `None` on platforms
/// where neither directory is present (the stat PV is then left at
/// its last value rather than reporting a misleading zero).
pub fn open_fd_count() -> Option<u64> {
    // `/proc/self/fd` is authoritative on Linux. `/dev/fd` works on
    // macOS and the BSDs (it is an fdescfs mount). Try the Linux path
    // first since on Linux `/dev/fd` is a symlink into procfs anyway.
    for dir in ["/proc/self/fd", "/dev/fd"] {
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                let n = entries.filter(|e| e.is_ok()).count() as u64;
                // `read_dir` itself holds one descriptor open for the
                // duration of the iteration; it is included in the
                // listing on Linux/procfs. Subtract it so the count
                // is the steady-state value. Saturate at 0 in the
                // (impossible) case of an empty directory.
                return Some(n.saturating_sub(1));
            }
            Err(_) => continue,
        }
    }
    None
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

    /// a `pvExistTest` (search resolution) must bump only the
    /// separate `exist_count` (C `existTestRate`), never the upstream
    /// event counters. Pre-fix the resolver called `record_event()`,
    /// inflating total_events / eventRate / clientEventCount with search
    /// traffic.
    #[test]
    fn exist_test_does_not_inflate_event_counters() {
        let stats = Stats::new("g:".into());
        stats.record_exist_test();
        stats.record_exist_test();
        stats.record_exist_test();
        assert_eq!(
            stats.exist_count.load(Ordering::Relaxed),
            3,
            "exist_count tracks pvExistTest resolutions"
        );
        assert_eq!(
            stats.total_events.load(Ordering::Relaxed),
            0,
            "pvExistTest must NOT touch total_events (clientEventCount/eventRate source)"
        );

        // And an upstream event bumps only total_events, not exist_count.
        stats.record_event();
        assert_eq!(stats.total_events.load(Ordering::Relaxed), 1);
        assert_eq!(stats.exist_count.load(Ordering::Relaxed), 3);
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
        assert!(db.has_name("g:existTestRate").await);
    }

    #[tokio::test]
    async fn empty_prefix_skips_publish() {
        let stats = Stats::new("".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;
        assert!(!db.has_name("totalPvs").await);
    }

    // --- B5: fd / RATE_STATS counters ---

    #[test]
    fn rate_stats_counters_increment() {
        let stats = Stats::new("g:".into());
        assert_eq!(stats.post_event_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.loop_count.load(Ordering::Relaxed), 0);

        stats.record_post_event();
        stats.record_post_event();
        stats.record_post_event();
        assert_eq!(stats.post_event_count.load(Ordering::Relaxed), 3);

        stats.record_loop();
        assert_eq!(stats.loop_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn open_fd_count_is_plausible() {
        // The test process always has at least stdin/stdout/stderr
        // open, so on any supported platform the count is non-zero.
        // On an unsupported platform `None` is acceptable.
        if let Some(n) = open_fd_count() {
            assert!(n >= 3, "expected at least 3 open fds, got {n}");
        }
    }

    #[test]
    fn open_fd_count_tracks_new_descriptors() {
        let before = match open_fd_count() {
            Some(n) => n,
            None => return, // unsupported platform — nothing to assert
        };
        // Open a batch of descriptors at once. A single fd would be
        // lost in the noise of a parallel test runner (other threads
        // open/close fds concurrently); a batch of 32 produces a
        // delta that comfortably exceeds that noise floor. The files
        // are held open in `_held` until the assertion runs.
        const BATCH: usize = 32;
        let dir = std::env::temp_dir();
        let mut _held = Vec::with_capacity(BATCH);
        let mut paths = Vec::with_capacity(BATCH);
        for i in 0..BATCH {
            let p = dir.join(format!("ca_gw_stats_fd_probe_{}_{i}", std::process::id()));
            _held.push(std::fs::File::create(&p).expect("create temp file"));
            paths.push(p);
        }
        let during = open_fd_count().expect("fd count available");
        // The count must have risen by at least half the batch even
        // if a few descriptors are transiently miscounted under
        // parallel test execution.
        assert!(
            during >= before + (BATCH as u64) / 2,
            "open fd count did not rise enough: before={before} during={during}"
        );
        drop(_held);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn publish_initial_creates_rate_stats_pvs() {
        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        assert!(db.has_name("g:fd").await);
        assert!(db.has_name("g:clientEventCount").await);
        assert!(db.has_name("g:postEventCount").await);
        assert!(db.has_name("g:loopCount").await);
    }

    #[tokio::test]
    async fn refresh_publishes_rate_stats() {
        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        // Drive the counters, then refresh and confirm the PVs reflect
        // the new values.
        stats.record_event(); // clientEventCount source
        stats.record_event();
        stats.record_post_event();
        stats.record_loop();
        stats.record_loop();
        stats.record_loop();

        let cache = RwLock::new(PvCache::new());
        stats.refresh(&cache, &db, 0, 0).await;

        assert_eq!(
            db.get_pv("g:clientEventCount").await.unwrap(),
            EpicsValue::Long(2)
        );
        assert_eq!(
            db.get_pv("g:postEventCount").await.unwrap(),
            EpicsValue::Long(1)
        );
        assert_eq!(db.get_pv("g:loopCount").await.unwrap(), EpicsValue::Long(3));
        // `fd` should have been posted with a plausible value on any
        // supported platform. It is a C gateStat PV → DBR_DOUBLE.
        if let Ok(EpicsValue::Double(fd)) = db.get_pv("g:fd").await {
            assert!(fd >= 0.0);
        }
    }

    /// `vctotal` must count only cache entries with an attached
    /// downstream client (C `total_vc`, gateVc.cc:406,472), NOT the
    /// whole cache. `pvtotal` (C `total_pv`, gatePv.cc:183) remains the
    /// cache size. Pre-fix both were posted from `cache_size`, so a
    /// gateway caching unsubscribed PVs over-reported its virtual
    /// channels.
    #[tokio::test]
    async fn refresh_vctotal_counts_only_subscribed_entries() {
        use super::super::cache::{GwPvEntry, PvState};

        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        let mut cache = PvCache::new();

        // Active entry with downstream subscribers → a virtual channel.
        let mut a = GwPvEntry::new_connecting("pvA");
        a.set_state(PvState::Active);
        a.add_subscriber(1);
        a.add_subscriber(2);
        cache.insert(a);

        // Connecting entry that already has a downstream client attached
        // → still a VC even though the upstream is not yet active.
        let mut b = GwPvEntry::new_connecting("pvB");
        b.add_subscriber(3);
        cache.insert(b);

        // Inactive entry with no downstream client → NOT a VC, but still
        // a cached PV counted by pvtotal.
        let mut c = GwPvEntry::new_connecting("pvC");
        c.set_state(PvState::Inactive);
        cache.insert(c);

        let cache_size = cache.len();
        let cache = RwLock::new(cache);
        stats.refresh(&cache, &db, cache_size, 0).await;

        // pvtotal = all cached PVs (3); vctotal = only the 2 with a
        // downstream client. Both served as DBR_DOUBLE (C STAT_DOUBLE).
        assert_eq!(
            db.get_pv("g:pvtotal").await.unwrap(),
            EpicsValue::Double(3.0)
        );
        assert_eq!(
            db.get_pv("g:vctotal").await.unwrap(),
            EpicsValue::Double(2.0)
        );
    }

    /// the `disconnected` alias must report the Disconnect-state
    /// count (C `statDisconnected`, gatePv.cc:607,616), NOT the Dead
    /// count; and `unconnected` (C `statUnconnected`) must include
    /// Disconnect alongside Connecting and Dead (gatePv.cc:315,328,608,
    /// 617). Pre-fix `disconnected` posted `dead` and `unconnected`
    /// dropped the Disconnect state entirely.
    #[tokio::test]
    async fn refresh_disconnected_and_unconnected_map_disconnect_state() {
        use super::super::cache::{GwPvEntry, PvState};

        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        let mut cache = PvCache::new();
        let mut add = |name: &str, state: PvState| {
            let mut e = GwPvEntry::new_connecting(name);
            e.set_state(state);
            cache.insert(e);
        };
        add("a", PvState::Active);
        add("b", PvState::Inactive);
        add("c", PvState::Connecting);
        add("d", PvState::Dead);
        add("e", PvState::Disconnect);
        add("f", PvState::Disconnect);

        let cache_size = cache.len();
        let cache = RwLock::new(cache);
        stats.refresh(&cache, &db, cache_size, 0).await;

        // disconnected = Disconnect-state count (2), distinct from dead (1).
        // C-name aliases are served as DBR_DOUBLE (C STAT_DOUBLE).
        assert_eq!(
            db.get_pv("g:disconnected").await.unwrap(),
            EpicsValue::Double(2.0)
        );
        assert_eq!(db.get_pv("g:dead").await.unwrap(), EpicsValue::Double(1.0));
        // unconnected = connecting(1) + dead(1) + disconnect(2) = 4.
        assert_eq!(
            db.get_pv("g:unconnected").await.unwrap(),
            EpicsValue::Double(4.0)
        );
        // connected = active(1) + inactive(1) = 2 (unchanged by this fix).
        assert_eq!(
            db.get_pv("g:connected").await.unwrap(),
            EpicsValue::Double(2.0)
        );
    }

    /// every C-name compat-alias stat PV must be served as
    /// DBR_DOUBLE (C ca-gateway `gateStat.cc:27` `#define STAT_DOUBLE` →
    /// `bestExternalType()` = aitEnumFloat64, gateStat.cc:235-238).
    /// Pre-fix they were registered as DBR_LONG, so a downstream client
    /// connecting against the Rust gateway saw a divergent native type
    /// at CREATE_CHANNEL. The native type is fixed at registration, so a
    /// client connecting before the first refresh already sees Double.
    /// Rust-native names (totalPvs, activeCount, …) keep DBR_LONG — they
    /// have no C-name counterpart.
    #[tokio::test]
    async fn compat_alias_stats_are_double_native_type() {
        let stats = Stats::new("g:".into());
        let db = PvDatabase::new();
        stats.publish_initial(&db).await;

        for name in [
            "g:vctotal",
            "g:pvtotal",
            "g:connected",
            "g:active",
            "g:inactive",
            "g:unconnected",
            "g:dead",
            "g:connecting",
            "g:disconnected",
            "g:fd",
        ] {
            assert!(
                matches!(db.get_pv(name).await.unwrap(), EpicsValue::Double(_)),
                "{name} must be DBR_DOUBLE to match C gateStat"
            );
        }

        // Rust-native names have no C counterpart and stay DBR_LONG.
        for name in [
            "g:totalPvs",
            "g:activeCount",
            "g:deadCount",
            "g:clientEventCount",
        ] {
            assert!(
                matches!(db.get_pv(name).await.unwrap(), EpicsValue::Long(_)),
                "{name} is Rust-native and must stay DBR_LONG"
            );
        }
    }
}
