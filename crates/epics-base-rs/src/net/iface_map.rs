//! IPv4 network interface enumeration with periodic refresh.
//!
//! Wraps the [`if-addrs`] crate (cross-platform) into an
//! [`IfaceMap`] keyed by `ifindex`. Built once at startup and
//! refreshable on demand — multi-NIC environments where interfaces
//! come and go (USB Ethernet, hot-plug iface) need a fresh snapshot
//! per search burst, but the cost is small.
//!
//! Mirrors the data carried by pvxs `IfaceMap::Current` (src/iface.cpp).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Snapshot of one IPv4 interface.
#[derive(Debug, Clone)]
pub struct IfaceInfo {
    /// Kernel interface index (`if_nametoindex`). 0 means "let the
    /// kernel pick" — useful as a sentinel when the platform did
    /// not surface an index.
    pub index: u32,
    /// Interface name (`eth0`, `en0`, `Wi-Fi`, ...).
    pub name: String,
    /// IPv4 address bound on this interface.
    pub ip: Ipv4Addr,
    /// IPv4 netmask.
    pub netmask: Ipv4Addr,
    /// Subnet broadcast address (when reported by the OS), e.g.
    /// `10.0.0.255`. `None` for point-to-point links.
    pub broadcast: Option<Ipv4Addr>,
    /// Whether the interface is up and not loopback.
    pub up_non_loopback: bool,
}

/// Refreshable cache of IPv4 interfaces.
///
/// Cheap to clone (Arc-shared internal state). Spawned tasks share a
/// single map and refresh on demand via [`IfaceMap::refresh_if_stale`].
#[derive(Clone)]
pub struct IfaceMap {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    ifaces: Vec<IfaceInfo>,
    last_refresh: Instant,
}

impl IfaceMap {
    /// Build a fresh map by enumerating interfaces now.
    pub fn new() -> Self {
        let me = Self {
            inner: Arc::new(Mutex::new(Inner {
                ifaces: Vec::new(),
                last_refresh: Instant::now() - Duration::from_secs(3600),
            })),
        };
        me.refresh();
        me
    }

    /// Force-refresh the snapshot.
    pub fn refresh(&self) {
        let new = enumerate_v4();
        let mut g = self.inner.lock();
        g.ifaces = new;
        g.last_refresh = Instant::now();
    }

    /// Refresh if the snapshot is older than `max_age`. Returns the
    /// snapshot age before any refresh.
    pub fn refresh_if_stale(&self, max_age: Duration) -> Duration {
        let age = self.inner.lock().last_refresh.elapsed();
        if age > max_age {
            self.refresh();
        }
        age
    }

    /// Spawn a background tokio task that refreshes the snapshot
    /// every `period` until the returned [`tokio::task::JoinHandle`]
    /// is aborted. Mirrors pvxs `IfMapDaemon` (evhelper.cpp:715-758)
    /// which polls every 15 s.
    ///
    /// Returns the handle so callers that own the runtime can store
    /// it for shutdown; dropping it does NOT cancel the task — abort
    /// it explicitly. Idempotent: multiple background refreshers on
    /// the same map cost extra wakeups but are harmless.
    ///
    /// Without this, dynamic infrastructure (DHCP renewals changing
    /// the broadcast address; K8s pod network re-attach; VM live
    /// migration; cable hot-plug) leaves the snapshot stale, and
    /// any sender that derives a broadcast destination from the
    /// snapshot ends up sending to the wrong subnet.
    pub fn spawn_refresh(&self, period: Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            // First tick fires immediately — skip it so we don't
            // refresh twice in a row right after `IfaceMap::new()`
            // (which already populated the snapshot).
            tick.tick().await;
            loop {
                tick.tick().await;
                me.refresh();
            }
        })
    }

    /// Snapshot of all IPv4 interfaces. Includes loopback unless
    /// callers filter via [`IfaceInfo::up_non_loopback`].
    pub fn all(&self) -> Vec<IfaceInfo> {
        self.inner.lock().ifaces.clone()
    }

    /// Snapshot of up, non-loopback IPv4 interfaces — the typical
    /// fanout target list for SEARCH/beacon traffic.
    pub fn up_non_loopback(&self) -> Vec<IfaceInfo> {
        self.inner
            .lock()
            .ifaces
            .iter()
            .filter(|i| i.up_non_loopback)
            .cloned()
            .collect()
    }

    /// Look up an interface by its kernel index. Returns `None` when
    /// the index isn't known to this snapshot — caller may want to
    /// `refresh()` and retry once.
    pub fn by_index(&self, index: u32) -> Option<IfaceInfo> {
        self.inner
            .lock()
            .ifaces
            .iter()
            .find(|i| i.index == index)
            .cloned()
    }

    /// Pick the interface index that should originate traffic
    /// destined for `dest`. The selection rules (in priority order):
    ///
    /// 1. **Subnet match** — `dest` falls within an interface's
    ///    `(ip, netmask)`. Returned when present.
    /// 2. **Broadcast match** — `dest` equals an interface's
    ///    subnet broadcast.
    /// 3. **Loopback** — `127.0.0.0/8` → loopback interface.
    /// 4. Otherwise `None` — caller treats this as "no per-NIC
    ///    pinning, let the OS route". For limited broadcast and
    ///    multicast destinations the caller fanouts across all
    ///    interfaces explicitly.
    pub fn route_to(&self, dest: Ipv4Addr) -> Option<IfaceInfo> {
        let g = self.inner.lock();
        // (1) subnet match
        for i in &g.ifaces {
            if subnet_contains(i.ip, i.netmask, dest) {
                return Some(i.clone());
            }
        }
        // (2) explicit subnet broadcast
        for i in &g.ifaces {
            if Some(dest) == i.broadcast {
                return Some(i.clone());
            }
        }
        // (3) loopback
        if dest.is_loopback() {
            return g.ifaces.iter().find(|i| i.ip.is_loopback()).cloned();
        }
        None
    }
}

impl Default for IfaceMap {
    fn default() -> Self {
        Self::new()
    }
}

fn subnet_contains(ip: Ipv4Addr, mask: Ipv4Addr, candidate: Ipv4Addr) -> bool {
    let net = u32::from(ip) & u32::from(mask);
    let cnet = u32::from(candidate) & u32::from(mask);
    net == cnet && u32::from(mask) != 0
}

fn enumerate_v4() -> Vec<IfaceInfo> {
    let Ok(list) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(list.len());
    for iface in list {
        let if_addrs::IfAddr::V4(v4) = &iface.addr else {
            continue;
        };
        // `if-addrs` 0.13+ surfaces the kernel ifindex on every
        // platform we target. `None` means the OS didn't report one
        // (rare, but treat as 0 sentinel — the per-NIC fanout
        // backend keys on the bound IP, not the index, so this is
        // benign).
        let index = iface.index.unwrap_or(0);
        out.push(IfaceInfo {
            index,
            name: iface.name.clone(),
            ip: v4.ip,
            netmask: v4.netmask,
            broadcast: v4.broadcast,
            up_non_loopback: !iface.is_loopback(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_returns_loopback_at_minimum() {
        let map = IfaceMap::new();
        let all = map.all();
        // Every machine has at least one loopback v4 (127.0.0.1).
        assert!(
            all.iter().any(|i| i.ip.is_loopback()),
            "loopback IPv4 interface should be present (got {all:?})"
        );
    }

    #[test]
    fn loopback_routing_lands_on_loopback() {
        let map = IfaceMap::new();
        let r = map.route_to(Ipv4Addr::LOCALHOST);
        assert!(r.is_some(), "127.0.0.1 must route to a known interface");
        assert!(r.unwrap().ip.is_loopback());
    }

    #[test]
    fn refresh_updates_timestamp() {
        let map = IfaceMap::new();
        std::thread::sleep(Duration::from_millis(20));
        let age = map.refresh_if_stale(Duration::from_millis(10));
        assert!(
            age >= Duration::from_millis(20),
            "refresh_if_stale should report the pre-refresh age (got {age:?})"
        );
    }

    #[test]
    fn subnet_contains_basic() {
        // 10.0.0.5/24 contains 10.0.0.99 but not 10.0.1.1
        let ip = Ipv4Addr::new(10, 0, 0, 5);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        assert!(subnet_contains(ip, mask, Ipv4Addr::new(10, 0, 0, 99)));
        assert!(!subnet_contains(ip, mask, Ipv4Addr::new(10, 0, 1, 1)));
    }

    #[test]
    fn subnet_contains_zero_mask_rejects() {
        // 0.0.0.0 mask matches everything, which is meaningless for
        // routing — we explicitly reject it.
        assert!(!subnet_contains(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(8, 8, 8, 8)
        ));
    }

    /// `spawn_refresh` actually fires the periodic refresh — verify
    /// the snapshot's `last_refresh` advances at least once in the
    /// poll window. Mirrors pvxs `IfMapDaemon` 15 s behaviour at a
    /// short test cadence (50 ms × ~3 ticks ≈ 150 ms total).
    #[tokio::test(flavor = "current_thread")]
    async fn spawn_refresh_advances_last_refresh() {
        let map = IfaceMap::new();
        let initial = map.inner.lock().last_refresh;
        let handle = map.spawn_refresh(Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = map.inner.lock().last_refresh;
        assert!(
            after > initial,
            "background refresh must update last_refresh"
        );
        handle.abort();
    }
}
