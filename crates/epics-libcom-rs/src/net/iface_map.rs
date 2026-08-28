//! IPv4 network interface enumeration with periodic refresh.
//!
//! Wraps the `if-addrs` crate (cross-platform) into an
//! [`IfaceMap`] keyed by `ifindex`. Built once at startup and
//! refreshable on demand — multi-NIC environments where interfaces
//! come and go (USB Ethernet, hot-plug iface) need a fresh snapshot
//! per search burst, but the cost is small.
//!
//! Mirrors the data carried by pvxs `IfaceMap::Current` (src/iface.cpp).

use std::io;
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
    /// `true` when the interface carries the OS `IFF_UP` flag **and**
    /// is not loopback — the eligibility test for SEARCH/beacon
    /// fanout. C parity: `osiSockDiscoverBroadcastAddresses`
    /// (`osdNetIfConf.c:170-181`) skips `!(IFF_UP)` and `IFF_LOOPBACK`
    /// interfaces. On Linux (where the crate links `libc`) this
    /// consults the live kernel flags via `getifaddrs`; on other
    /// targets it falls back to `!is_loopback()` because the
    /// platform's enumerator already excludes operationally-down
    /// adapters.
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
    ///
    /// Fails when the OS enumeration fails, so an empty map means exactly
    /// one thing — the host reported no IPv4 interfaces. It used to mean
    /// that *or* that `getifaddrs` had errored, and every caller that asks
    /// "is there an external NIC here?" read the two as the same answer.
    pub fn new() -> io::Result<Self> {
        let me = Self {
            inner: Arc::new(Mutex::new(Inner {
                ifaces: Vec::new(),
                // Overwritten by `refresh()` on the next line, so any valid
                // Instant works. Must NOT back-date with `Instant - Duration`:
                // that panics on Windows (where `Instant` is QPC-since-boot)
                // whenever the machine's uptime is shorter than the span.
                last_refresh: Instant::now(),
            })),
        };
        me.refresh()?;
        Ok(me)
    }

    /// Force-refresh the snapshot.
    ///
    /// The single writer of `Inner.ifaces`, and it writes only what a
    /// successful enumeration returned: on failure the previous snapshot
    /// stands rather than being replaced by an empty one, so a transient
    /// `getifaddrs` error cannot blank the fanout list under a running
    /// sender.
    pub fn refresh(&self) -> io::Result<()> {
        let new = enumerate_v4()?;
        let mut g = self.inner.lock();
        g.ifaces = new;
        g.last_refresh = Instant::now();
        Ok(())
    }

    /// Refresh if the snapshot is older than `max_age`. Returns the
    /// snapshot age before any refresh.
    pub fn refresh_if_stale(&self, max_age: Duration) -> io::Result<Duration> {
        let age = self.inner.lock().last_refresh.elapsed();
        if age > max_age {
            self.refresh()?;
        }
        Ok(age)
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
    pub fn spawn_refresh(
        &self,
        reactor: &crate::runtime::task::Reactor,
        period: Duration,
    ) -> crate::runtime::task::TaskHandle<()> {
        let me = self.clone();
        reactor.spawn(async move {
            let mut tick = crate::runtime::task::interval(period);
            // First tick fires immediately — skip it so we don't
            // refresh twice in a row right after `IfaceMap::new()`
            // (which already populated the snapshot).
            tick.tick().await;
            loop {
                tick.tick().await;
                // Keep the previous snapshot when enumeration fails and let
                // the next tick retry — a periodic refresher must not be
                // able to turn a momentary OS error into "this host has no
                // interfaces" for everyone reading the map.
                if let Err(e) = me.refresh() {
                    tracing::debug!("iface map refresh failed, keeping previous snapshot: {e}");
                }
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
    /// 4. **Default route** — an interface with a `0.0.0.0` netmask
    ///    matches any destination; used only as a fallback so it never
    ///    shadows a specific subnet match.
    /// 5. Otherwise `None` — caller treats this as "no per-NIC
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
        // (4) default-route interface — a `0.0.0.0` netmask matches
        // every destination. `subnet_contains` rejects it in pass (1)
        // so it never shadows a specific subnet; here it is the
        // explicit fallback for an otherwise-unrouted dest.
        if let Some(i) = g
            .ifaces
            .iter()
            .find(|i| !i.ip.is_loopback() && u32::from(i.netmask) == 0)
        {
            return Some(i.clone());
        }
        None
    }
}

fn subnet_contains(ip: Ipv4Addr, mask: Ipv4Addr, candidate: Ipv4Addr) -> bool {
    let net = u32::from(ip) & u32::from(mask);
    let cnet = u32::from(candidate) & u32::from(mask);
    net == cnet && u32::from(mask) != 0
}

/// Per-interface-name OS flag snapshot used to compute
/// `up_non_loopback`. C parity: the `IFF_UP` / `IFF_LOOPBACK` checks in
/// `osiSockDiscoverBroadcastAddresses` (`osdNetIfConf.c:170-181`).
#[cfg(target_os = "linux")]
fn interface_up_flags() -> std::collections::HashMap<String, bool> {
    use std::collections::HashMap;
    use std::ffi::CStr;

    let mut map: HashMap<String, bool> = HashMap::new();
    // SAFETY: standard getifaddrs / freeifaddrs pairing; the pointer is
    // only dereferenced while non-null and freed exactly once.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 || head.is_null() {
            return map;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_name.is_null() {
                if let Ok(name) = CStr::from_ptr(ifa.ifa_name).to_str() {
                    // C: skip interfaces without IFF_UP, skip IFF_LOOPBACK.
                    let flags = ifa.ifa_flags as libc::c_int;
                    let up = (flags & libc::IFF_UP) != 0;
                    let loopback = (flags & libc::IFF_LOOPBACK) != 0;
                    let eligible = up && !loopback;
                    // An interface can have several addresses; if any
                    // entry reports it up+non-loopback, keep that.
                    map.entry(name.to_string())
                        .and_modify(|e| *e |= eligible)
                        .or_insert(eligible);
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    map
}

fn enumerate_v4() -> io::Result<Vec<IfaceInfo>> {
    let list = if_addrs::get_if_addrs()?;
    // C parity: on Linux consult the live kernel `IFF_UP`/`IFF_LOOPBACK`
    // flags via `getifaddrs` so an administratively-down interface that
    // still has an IPv4 address configured is not reported as a fanout
    // target. (Linux-only because `getifaddrs`/`SIOCGIFFLAGS` is the Linux
    // path — the crate itself links `libc` on every unix.)
    #[cfg(target_os = "linux")]
    let up_flags = interface_up_flags();

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

        // C `osiSockDiscoverBroadcastAddresses`: an interface is a
        // fanout target only when it is `IFF_UP` and not loopback.
        #[cfg(target_os = "linux")]
        let up_non_loopback = match up_flags.get(&iface.name) {
            // Kernel flags known — honour them (and never treat
            // loopback as a fanout target even if a stale flag map
            // somehow disagrees).
            Some(&eligible) => eligible && !iface.is_loopback(),
            // Interface absent from the getifaddrs snapshot (raced a
            // hot-unplug, or getifaddrs failed) — fall back to the
            // loopback test rather than dropping the interface.
            None => !iface.is_loopback(),
        };
        // Non-Linux (macOS / Windows / *BSD): the crate does not link
        // `libc` here, and the platform enumerator already excludes
        // operationally-down adapters, so the loopback test is the
        // best portable approximation.
        #[cfg(not(target_os = "linux"))]
        let up_non_loopback = !iface.is_loopback();

        out.push(IfaceInfo {
            index,
            name: iface.name.clone(),
            ip: v4.ip,
            netmask: v4.netmask,
            broadcast: v4.broadcast,
            up_non_loopback,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_returns_loopback_at_minimum() {
        let map = IfaceMap::new().expect("getifaddrs must succeed on the host");
        let all = map.all();
        // Every machine has at least one loopback v4 (127.0.0.1).
        assert!(
            all.iter().any(|i| i.ip.is_loopback()),
            "loopback IPv4 interface should be present (got {all:?})"
        );
    }

    #[test]
    fn loopback_routing_lands_on_loopback() {
        let map = IfaceMap::new().expect("getifaddrs must succeed on the host");
        let r = map.route_to(Ipv4Addr::LOCALHOST);
        assert!(r.is_some(), "127.0.0.1 must route to a known interface");
        assert!(r.unwrap().ip.is_loopback());
    }

    #[test]
    fn refresh_updates_timestamp() {
        let map = IfaceMap::new().expect("getifaddrs must succeed on the host");
        std::thread::sleep(Duration::from_millis(20));
        let age = map
            .refresh_if_stale(Duration::from_millis(10))
            .expect("getifaddrs must succeed on the host");
        assert!(
            age >= Duration::from_millis(20),
            "refresh_if_stale should report the pre-refresh age (got {age:?})"
        );
    }

    /// M5 C-parity: loopback is never reported as a fanout target,
    /// and every interface flagged `up_non_loopback` is genuinely
    /// non-loopback (C `osiSockDiscoverBroadcastAddresses` skips
    /// `IFF_LOOPBACK`).
    #[test]
    fn up_non_loopback_excludes_loopback() {
        let map = IfaceMap::new().expect("getifaddrs must succeed on the host");
        for iface in map.all() {
            if iface.ip.is_loopback() {
                assert!(
                    !iface.up_non_loopback,
                    "loopback {iface:?} must not be a fanout target"
                );
            }
        }
        // Everything in up_non_loopback() must be non-loopback.
        for iface in map.up_non_loopback() {
            assert!(
                !iface.ip.is_loopback(),
                "up_non_loopback() must not surface loopback: {iface:?}"
            );
        }
    }

    /// M5 C-parity: on Linux the live `IFF_UP` kernel flag is consulted.
    /// The loopback interface is `IFF_UP` but `IFF_LOOPBACK`, so the
    /// flag map reports it as ineligible.
    #[cfg(target_os = "linux")]
    #[test]
    fn interface_up_flags_marks_loopback_ineligible() {
        let flags = interface_up_flags();
        // `lo`/`lo0` is up but loopback -> ineligible. Accept either
        // name; the machine must have at least one loopback entry.
        let lo_ineligible = flags
            .iter()
            .any(|(name, &eligible)| (name == "lo" || name == "lo0") && !eligible);
        assert!(
            lo_ineligible || flags.is_empty(),
            "loopback must be flagged ineligible in the IFF_UP map: {flags:?}"
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
    #[epics_macros_rs::epics_test]
    async fn spawn_refresh_advances_last_refresh() {
        let map = IfaceMap::new().expect("getifaddrs must succeed on the host");
        let initial = map.inner.lock().last_refresh;
        let reactor =
            crate::runtime::task::Reactor::current().expect("the test driver enters an executor");
        let handle = map.spawn_refresh(&reactor, Duration::from_millis(50));
        crate::runtime::task::sleep(Duration::from_millis(200)).await;
        let after = map.inner.lock().last_refresh;
        assert!(
            after > initial,
            "background refresh must update last_refresh"
        );
        handle.abort();
    }
}
