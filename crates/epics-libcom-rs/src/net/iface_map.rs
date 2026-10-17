//! IPv4 network interface enumeration with periodic refresh.
//!
//! The up interfaces of [`super::iface_v4::enumerate`] (`GetAdaptersAddresses`
//! on Windows) as an [`IfaceMap`] keyed by `ifindex`. Built once at startup and
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
    /// `ifa_broadaddr` as the OS reported it, e.g. `10.0.0.255`: where C
    /// and pvxs send to reach this NIC's segment. `None` for point-to-point
    /// links. Not necessarily a local address — see [`Self::rx_broadcast`].
    pub broadcast: Option<Ipv4Addr>,
    /// `true` when the interface is not loopback — the eligibility test for
    /// SEARCH/beacon fanout, C `osiSockDiscoverBroadcastAddresses`
    /// (`osdNetIfConf.c:170-181`). The `IFF_UP` half holds for every entry:
    /// a down interface is not enumerated.
    pub up_non_loopback: bool,
}

impl IfaceInfo {
    /// The local address this NIC's subnet broadcasts arrive at, which a
    /// socket can bind to receive them.
    ///
    /// Not [`Self::broadcast`] on Linux: with `IFF_BROADCAST` set, glibc
    /// fills `ifa_broadaddr` with the peer of a `peer` address, or with the
    /// NIC's own address when no `brd` was given, and binding either fails.
    /// The kernel installs a local broadcast route for `prefix | ~mask` on
    /// every prefix shorter than /31 (`fib_add_ifaddr`), so that is the
    /// address there.
    pub fn rx_broadcast(&self) -> Option<Ipv4Addr> {
        if self.ip.is_loopback() {
            return None;
        }
        #[cfg(target_os = "linux")]
        {
            let mask = u32::from(self.netmask);
            (mask != 0 && mask.leading_ones() < 31)
                .then(|| Ipv4Addr::from(u32::from(self.ip) | !mask))
        }
        #[cfg(not(target_os = "linux"))]
        self.broadcast.filter(|b| !b.is_unspecified())
    }
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

/// The up IPv4 interfaces, from the one walker every target enumerates
/// through ([`super::iface_v4::enumerate`]), which drops a down NIC.
#[cfg(unix)]
fn enumerate_v4() -> io::Result<Vec<IfaceInfo>> {
    Ok(super::iface_v4::enumerate()?
        .into_iter()
        .map(|i| IfaceInfo {
            index: std::ffi::CString::new(i.name.as_str())
                // SAFETY: a NUL-terminated string that outlives the call.
                .map_or(0, |name| unsafe { libc::if_nametoindex(name.as_ptr()) }),
            up_non_loopback: !i.is_loopback() && !i.ip.is_loopback(),
            broadcast: i.dest.filter(|_| i.is_broadcast()),
            netmask: i.netmask.unwrap_or(Ipv4Addr::UNSPECIFIED),
            ip: i.ip,
            name: i.name,
        })
        .collect())
}

/// Windows: the up adapters of pvxs's `IfaceMap::_refresh` walk, which
/// carries the index the unix path asks `if_nametoindex` for.
#[cfg(not(unix))]
fn enumerate_v4() -> io::Result<Vec<IfaceInfo>> {
    let mut out = Vec::new();
    for a in super::win_adapters::up_adapter_addrs()? {
        let std::net::IpAddr::V4(ip) = a.ip else {
            continue;
        };
        out.push(IfaceInfo {
            index: a.index,
            netmask: a.netmask_v4(),
            broadcast: a.broadcast_v4(),
            up_non_loopback: !a.loopback && !ip.is_loopback(),
            name: a.name,
            ip,
        });
    }
    Ok(out)
}

/// One interface of pvxs `IfaceMap::all_external()`, with what a multicast
/// join on it takes: `index_of(name)` for v6 and `address_of(name)` for v4
/// (`config.cpp:97-117`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIface {
    /// Interface name.
    pub name: String,
    /// Kernel interface index, 0 when the OS reported none.
    pub index: u32,
    /// The lowest IPv4 address, `address_of`'s first in pvxs's address
    /// order; `None` on a v6-only interface, whose v4 join takes
    /// `INADDR_ANY`.
    pub v4: Option<Ipv4Addr>,
    /// The lowest IPv6 address, which pvxs's `address_of` returns for a
    /// v6-only interface.
    pub v6: Option<std::net::Ipv6Addr>,
}

/// pvxs `IfaceMap::all_external()` (`evhelper.cpp:898-905`): every up
/// interface with an IPv4 or IPv6 address, loopback included, in name
/// order (`osdSockExt.cpp:259-315`). Walks `getifaddrs` itself on unix so
/// that an interface with only an `fe80::` address is kept.
pub fn all_external() -> io::Result<Vec<ExternalIface>> {
    let mut by_name = std::collections::BTreeMap::<String, ExternalIface>::new();
    let mut add = |name: &str, index: u32, ip: std::net::IpAddr| {
        let entry = by_name
            .entry(name.to_string())
            .or_insert_with(|| ExternalIface {
                name: name.to_string(),
                index,
                v4: None,
                v6: None,
            });
        match ip {
            std::net::IpAddr::V4(ip) => entry.v4 = Some(entry.v4.map_or(ip, |cur| cur.min(ip))),
            std::net::IpAddr::V6(ip) => entry.v6 = Some(entry.v6.map_or(ip, |cur| cur.min(ip))),
        }
    };
    #[cfg(unix)]
    // SAFETY: standard getifaddrs / freeifaddrs pairing; every pointer is
    // dereferenced only while non-null, and the list is freed exactly once.
    unsafe {
        use std::ffi::CStr;
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            let family = libc::c_int::from((*ifa.ifa_addr).sa_family);
            if family != libc::AF_INET && family != libc::AF_INET6 {
                continue;
            }
            if (ifa.ifa_flags as libc::c_int & libc::IFF_UP) == 0 {
                continue;
            }
            let index = libc::if_nametoindex(ifa.ifa_name);
            let Ok(name) = CStr::from_ptr(ifa.ifa_name).to_str() else {
                continue;
            };
            if index == 0 || name.is_empty() {
                continue;
            }
            let ip = if family == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).into()
            } else {
                let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr).into()
            };
            add(name, index, ip);
        }
        libc::freeifaddrs(head);
    }
    #[cfg(not(unix))]
    for a in super::win_adapters::up_adapter_addrs()? {
        add(&a.name, a.index, a.ip);
    }
    Ok(by_name.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn nic(ip: [u8; 4], prefix: u32, broadcast: Option<[u8; 4]>) -> IfaceInfo {
        IfaceInfo {
            index: 2,
            name: "d0".to_string(),
            ip: ip.into(),
            netmask: Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix).unwrap_or(0)),
            broadcast: broadcast.map(Ipv4Addr::from),
            up_non_loopback: true,
        }
    }

    /// One case per `ifa_broadaddr` shape `getifaddrs` gives an
    /// `IFF_BROADCAST` NIC on Linux, as `ip addr add` configured it.
    #[cfg(target_os = "linux")]
    #[test]
    fn rx_broadcast_is_the_kernel_local_broadcast() {
        let cases = [
            // `brd +`
            (
                nic([10, 6, 0, 1], 24, Some([10, 6, 0, 255])),
                Some([10, 6, 0, 255]),
            ),
            // no `brd`: glibc reports the NIC's own address
            (
                nic([10, 7, 0, 1], 24, Some([10, 7, 0, 1])),
                Some([10, 7, 0, 255]),
            ),
            // `peer 10.8.0.2`: the peer, and no subnet broadcast at /32
            (nic([10, 8, 0, 1], 32, Some([10, 8, 0, 2])), None),
            // `peer 10.5.0.2/24`: the peer, and the kernel's /24 broadcast
            (
                nic([10, 5, 0, 1], 24, Some([10, 5, 0, 2])),
                Some([10, 5, 0, 255]),
            ),
            // /31 has no broadcast (RFC 3021)
            (nic([10, 4, 0, 0], 31, Some([10, 4, 0, 1])), None),
            (nic([10, 3, 0, 1], 0, Some([255, 255, 255, 255])), None),
            (nic([127, 0, 0, 1], 8, None), None),
        ];
        for (info, want) in cases {
            assert_eq!(info.rx_broadcast(), want.map(Ipv4Addr::from), "{info:?}");
        }
    }

    /// pvxs's set: loopback is in it, with its v4 address and an index, and
    /// the names are in order, each once.
    #[test]
    fn all_external_includes_loopback_in_name_order() {
        let all = all_external().expect("getifaddrs must succeed on the host");
        let lo = all
            .iter()
            .find(|i| i.v4 == Some(Ipv4Addr::LOCALHOST))
            .unwrap_or_else(|| panic!("loopback missing: {all:?}"));
        assert_ne!(lo.index, 0, "{lo:?}");
        assert!(all.windows(2).all(|w| w[0].name < w[1].name), "{all:?}");
    }

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
