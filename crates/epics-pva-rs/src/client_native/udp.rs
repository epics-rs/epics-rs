//! Core UDP collector — the shared receive side of PVA UDP discovery.
//!
//! Ports pvxs's `UDPManager` / `UDPCollector` (`src/udp_collector.cpp`).
//! The design rule the whole module exists to enforce is **one wildcard
//! socket per `(address-family, port)`**, shared across every destination
//! on that port, with each received datagram's *original destination*
//! recovered from `IP_PKTINFO` / `IP_RECVDSTADDR` (v4) or `IPV6_PKTINFO`
//! (v6) and used to fan the datagram out only to the listeners that asked
//! for that destination.
//!
//! pvxs `udp_collector.cpp:140-151`:
//!
//! > Always bind to wildcard to receive all uni/broad/multicast […] we
//! > take the least common denominator across all platforms, which is to
//! > bind to the wildcard.
//!
//! Consequences mirrored here:
//!
//! * A broadcast (`255.255.255.255:5076`) or multicast (`224.0.2.3:5076`)
//!   listener is **never bound to its own address** — it is served by the
//!   wildcard collector for that port plus a group join. Binding the
//!   broadcast/multicast address directly is not portable and is exactly
//!   what pvxs avoids.
//! * Two listeners on the same `(family, port)` — say `255.255.255.255`
//!   and a specific NIC unicast — share a single [`UdpCollector`]
//!   (`UDPManager::Pvt::collect`, `udp_collector.cpp:102-121`, keyed by
//!   `std::make_pair(family, port)`).
//! * A multicast listener triggers a per-group `mcast_join`
//!   (`udp_collector.cpp:186-196`); a unicast/broadcast listener does not.
//! * Each decoded datagram is delivered to a listener `L` iff
//!   `L.dest.is_unspecified() || L.dest == orig_dest`
//!   (`udp_collector.cpp:451`, `:484`).
//!
//! Decoding SEARCH vs BEACON is left to the consumer: the collector hands
//! each matching listener the raw datagram plus its recovered original
//! destination, which is all the fan-out predicate needs. This keeps the
//! receive core independent of the wire codec.

// (1 live-UDP recv_loop test carries its own feature gate below — now
// redundant, and kept only so the census marker's count stays the reviewable
// number it was; §4.2 UDP search, stage 3.)

// RTEMS-EXEC-MODEL-ALLOW(5): this file does not build on the exec backend at
// all — `client_native/mod.rs` declares it `#[cfg(tokio_backend)]` because its
// `UdpSocket::readable` waits need a reactor the exec backend has not got. The
// five are counted, not run.
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use epics_base_rs::net::{PortOrigin, PortUse};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::Endpoint;

/// A datagram received by a [`UdpCollector`], delivered to every listener
/// whose requested destination matches.
#[derive(Debug, Clone)]
pub struct CollectedDatagram {
    /// UDP source address (the sender).
    pub src: SocketAddr,
    /// The datagram's original destination IP, recovered from the
    /// `IP_PKTINFO` / `IP_RECVDSTADDR` / `IPV6_PKTINFO` ancillary data
    /// (pvxs `recvfromx … &origDest`, `udp_collector.cpp:241`). `None`
    /// when the platform supplied no ancillary destination.
    pub orig_dest: Option<IpAddr>,
    /// Raw datagram bytes (one PVA UDP message, still to be decoded).
    pub data: Vec<u8>,
}

/// One registered listener: the destination it cares about plus the
/// channel the collector pushes matching datagrams into.
struct ListenerSlot {
    /// The requested destination IP. An unspecified address (`0.0.0.0` /
    /// `::`) is pvxs's `isAny()` wildcard — it matches every datagram.
    dest_ip: IpAddr,
    tx: mpsc::Sender<CollectedDatagram>,
}

/// Shared inner state of a collector: the wildcard socket, the joined
/// multicast groups, and the active listeners. Held behind an `Arc` so the
/// background receive task can observe its liveness via a `Weak`.
struct CollectorState {
    /// The wildcard bind address (`0.0.0.0:port` or `[::]:port`). Never a
    /// broadcast/multicast/unicast destination — that is the whole point.
    bind_addr: SocketAddr,
    sock: Arc<UdpSocket>,
    /// Multicast groups already joined on `sock`, deduplicated so a second
    /// listener on the same group does not re-join (pvxs `mcast_grps`).
    mcast: Mutex<HashSet<IpAddr>>,
    listeners: Mutex<Vec<ListenerSlot>>,
}

/// A reference-counted handle to a wildcard UDP collector. Cloning shares
/// the same socket and listener set; the background receive task lives as
/// long as at least one handle (or the owning [`UdpManager`] entry) does.
#[derive(Clone)]
pub struct UdpCollector {
    state: Arc<CollectorState>,
}

impl UdpCollector {
    /// The wildcard address this collector is bound to. Always an
    /// unspecified IP with the collector's port — assert-grade evidence
    /// that no destination address was bound directly.
    pub fn bind_addr(&self) -> SocketAddr {
        self.state.bind_addr
    }

    /// The actual bound socket address (resolves the assigned port when the
    /// collector was created for a port-0 request).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.state.sock.local_addr()
    }

    /// Register interest in `dest`. The returned receiver yields every
    /// datagram whose original destination matches `dest` (or all
    /// datagrams when `dest` is the unspecified wildcard).
    ///
    /// A multicast `dest` joins the group on this collector's wildcard
    /// socket (pvxs `addListener` → `mcast_join`, `udp_collector.cpp:186-196`);
    /// honouring any `@iface` modifier so the group is received on that one
    /// interface. A broadcast or unicast `dest` joins nothing — it is
    /// already served by the wildcard bind.
    pub fn add_listener(&self, dest: &Endpoint) -> io::Result<mpsc::Receiver<CollectedDatagram>> {
        let dest_ip = dest.addr.ip();
        if dest_ip.is_multicast() {
            self.join_multicast(dest)?;
        }
        // 64 buffered datagrams per listener: discovery traffic is bursty
        // but small, and a slow consumer drops the oldest rather than
        // stalling the shared receive loop.
        let (tx, rx) = mpsc::channel(64);
        self.state
            .listeners
            .lock()
            .expect("collector listeners mutex poisoned")
            .push(ListenerSlot { dest_ip, tx });
        Ok(rx)
    }

    /// Join the multicast group named by `dest`, deduplicated against the
    /// groups already joined on this socket.
    fn join_multicast(&self, dest: &Endpoint) -> io::Result<()> {
        let group = dest.addr.ip();
        {
            let joined = self
                .state
                .mcast
                .lock()
                .expect("collector mcast mutex poisoned");
            if joined.contains(&group) {
                return Ok(());
            }
        }
        // pvxs `SockEndpoint::resolve` (`config.cpp:88-120`): the `@iface`
        // interface, the wildcard when it names none or none that resolves.
        let ifmap = crate::config::IfaceMap::for_endpoints(std::iter::once(dest));
        match (group, dest.addr) {
            (IpAddr::V4(g), SocketAddr::V4(_)) => {
                self.state
                    .sock
                    .join_multicast_v4(g, dest.mcast_iface_v4(&ifmap))?;
            }
            (IpAddr::V6(g), SocketAddr::V6(_)) => {
                self.state
                    .sock
                    .join_multicast_v6(&g, dest.mcast_iface_v6(&ifmap))?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multicast group family does not match collector family",
                ));
            }
        }
        self.state
            .mcast
            .lock()
            .expect("collector mcast mutex poisoned")
            .insert(group);
        debug!(group = %group, "UDP collector joined multicast group");
        Ok(())
    }

    /// Number of multicast groups currently joined (test/inspection).
    pub fn joined_group_count(&self) -> usize {
        self.state
            .mcast
            .lock()
            .expect("collector mcast mutex poisoned")
            .len()
    }

    /// Number of active listeners (test/inspection).
    pub fn listener_count(&self) -> usize {
        self.state
            .listeners
            .lock()
            .expect("collector listeners mutex poisoned")
            .len()
    }
}

/// The shared owner of all wildcard collectors, one per `(family, port)`.
///
/// Mirrors pvxs `UDPManager::Pvt`: `collect()` reuses an existing collector
/// for a `(family, port)` pair, or creates one on demand
/// (`udp_collector.cpp:102-121`). Collectors are tracked by `Weak`, so the
/// background receive task and its socket are released once the last
/// [`UdpCollector`] handle for that key is dropped.
pub struct UdpManager {
    collectors: Mutex<HashMap<(bool, u16), Weak<CollectorState>>>,
}

impl Default for UdpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UdpManager {
    pub fn new() -> Self {
        Self {
            collectors: Mutex::new(HashMap::new()),
        }
    }

    /// Obtain the collector serving `dest`'s `(family, port)`, creating and
    /// starting it on first use.
    ///
    /// pvxs `collect()` reuses by `(family, port)` only when the port is
    /// non-zero; a port-0 request always binds a fresh ephemeral collector
    /// and is then cached under its *assigned* port. A unicast, broadcast,
    /// and multicast destination that share one port all resolve to the
    /// same collector — none of them is bound to its own address.
    pub fn collect(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
        dest: &Endpoint,
    ) -> io::Result<UdpCollector> {
        let is_v6 = matches!(dest.addr, SocketAddr::V6(_));
        let port = dest.addr.port();

        let mut map = self
            .collectors
            .lock()
            .expect("UdpManager collectors mutex poisoned");

        if port != 0 {
            if let Some(weak) = map.get(&(is_v6, port)) {
                if let Some(state) = weak.upgrade() {
                    return Ok(UdpCollector { state });
                }
            }
        }

        let state = CollectorState::bind_and_spawn(reactor, is_v6, port)?;
        // Cache under the *bound* port so a port-0 request is reusable by a
        // later collect on the assigned port (pvxs keys on `bind_addr.port()`).
        let bound_port = state.bind_addr.port();
        map.insert((is_v6, bound_port), Arc::downgrade(&state));
        Ok(UdpCollector { state })
    }
}

impl CollectorState {
    /// Bind one wildcard socket for `(family, port)`, enable original-
    /// destination ancillary data, and spawn the receive/fan-out task.
    fn bind_and_spawn(
        reactor: &epics_base_rs::runtime::task::Reactor,
        is_v6: bool,
        port: u16,
    ) -> io::Result<Arc<CollectorState>> {
        let domain = if is_v6 { Domain::IPV6 } else { Domain::IPV4 };
        let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        // `epicsSocketEnableAddressUseForDatagramFanout` (pvxs
        // `udp_collector.cpp:136`), applied at the bind below, so this
        // wildcard listener can coexist with a co-located server's UDP
        // responder and with sibling collectors on the same port.
        if is_v6 {
            // Keep the v6 collector v6-only; the v4 collector owns v4.
            sock.set_only_v6(true)?;
        } else {
            // Receive limited/subnet broadcasts on the wildcard socket.
            sock.set_broadcast(true)?;
        }
        sock.set_nonblocking(true)?;

        // Ask the kernel to attach each datagram's original destination as
        // ancillary data (pvxs `sock.enable_IP_PKTINFO()`,
        // `udp_collector.cpp:138`).
        #[cfg(any(unix, windows))]
        epics_base_rs::net::udp_rx::enable(&sock, is_v6)?;

        let bind_addr: SocketAddr = if is_v6 {
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
        } else {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
        };
        PortUse::Shared.bind(&sock, bind_addr, PortOrigin::Configured)?;

        let std_sock: std::net::UdpSocket = sock.into();
        let tokio_sock = UdpSocket::from_std(std_sock)?;
        // Resolve the wildcard bind address with the actually-assigned port
        // (matters for a port-0 ephemeral request).
        let bind_addr = tokio_sock.local_addr()?;
        let sock = Arc::new(tokio_sock);

        let state = Arc::new(CollectorState {
            bind_addr,
            sock: sock.clone(),
            mcast: Mutex::new(HashSet::new()),
            listeners: Mutex::new(Vec::new()),
        });

        // The receive task holds only a clone of the socket and a `Weak` to
        // the state, so it neither keeps the collector alive nor leaks the
        // socket: when the last handle drops, the next idle re-check (or the
        // post-datagram upgrade) fails and the task exits.
        reactor.spawn(recv_loop(sock, Arc::downgrade(&state)));

        Ok(state)
    }
}

/// Fan a freshly received datagram out to the matching listeners. A slot
/// whose receiver has been dropped is pruned; a slot whose buffer is full
/// drops this datagram but is kept (discovery is best-effort).
fn fanout(state: &CollectorState, src: SocketAddr, orig_dest: Option<IpAddr>, data: &[u8]) {
    let mut listeners = state
        .listeners
        .lock()
        .expect("collector listeners mutex poisoned");
    if listeners.is_empty() {
        return;
    }
    listeners.retain(|slot| {
        if !dest_matches(slot.dest_ip, orig_dest) {
            return true;
        }
        let datagram = CollectedDatagram {
            src,
            orig_dest,
            data: data.to_vec(),
        };
        match slot.tx.try_send(datagram) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    });
}

/// pvxs fan-out predicate (`udp_collector.cpp:451`, `:484`):
/// `dest.isAny() || dest == origDest`. The unspecified wildcard matches
/// every datagram; a specific destination matches only datagrams whose
/// recovered original destination equals it. Ports are not compared — every
/// listener on a collector shares the collector's port by construction.
fn dest_matches(dest_ip: IpAddr, orig_dest: Option<IpAddr>) -> bool {
    if dest_ip.is_unspecified() {
        return true;
    }
    match orig_dest {
        Some(od) => od == dest_ip,
        None => false,
    }
}

/// Background loop: receive datagrams on the wildcard socket and fan them
/// out. Exits when the owning [`CollectorState`] has been dropped.
async fn recv_loop(sock: Arc<UdpSocket>, weak: Weak<CollectorState>) {
    // PVA UDP messages fit well under 64 KiB; one reusable buffer.
    let mut buf = vec![0u8; 0x1_0000];
    loop {
        // Wake on readability, but bound the wait so an idle collector still
        // notices that its last handle was dropped and can exit.
        match epics_base_rs::runtime::task::timeout(Duration::from_secs(5), sock.readable()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break,
            Err(_elapsed) => {
                if weak.strong_count() == 0 {
                    break;
                }
                continue;
            }
        }

        match recv_with_orig_dest(&sock, &mut buf) {
            Ok(Some((n, src, orig_dest))) => {
                let Some(state) = weak.upgrade() else { break };
                fanout(&state, src, orig_dest, &buf[..n]);
            }
            // Spurious wakeup / would-block: re-arm.
            Ok(None) => {}
            Err(e) => {
                debug!(error = %e, "UDP collector recv error; stopping");
                break;
            }
        }
    }
}

/// Drive one non-blocking receive, returning the byte count, source address,
/// and recovered original destination ([`epics_base_rs::net::udp_rx`]). `Ok(None)` means
/// the read would block (re-arm and wait).
#[cfg(any(unix, windows))]
fn recv_with_orig_dest(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, Option<IpAddr>)>> {
    match sock.try_io(tokio::io::Interest::READABLE, || {
        epics_base_rs::net::udp_rx::recv(sock, buf)
    }) {
        Ok(d) => Ok(Some((d.len, d.src, d.dest))),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

/// Other targets: receive without ancillary data.
/// Yields the source address and `None` for the original destination.
/// `try_recv_from` mirrors the `WouldBlock` -> `Ok(None)` contract.
#[cfg(not(any(unix, windows)))]
fn recv_with_orig_dest(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, Option<IpAddr>)>> {
    match sock.try_recv_from(buf) {
        Ok((n, src)) => Ok(Some((n, src, None))),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(addr: &str) -> Endpoint {
        Endpoint {
            addr: addr.parse().unwrap(),
            ttl: None,
            iface: None,
        }
    }

    #[test]
    fn dest_matches_wildcard_accepts_everything() {
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert!(dest_matches(any, Some("10.0.0.1".parse().unwrap())));
        assert!(dest_matches(any, Some("224.0.2.3".parse().unwrap())));
        // A wildcard listener even matches when no orig-dest was recovered.
        assert!(dest_matches(any, None));
    }

    #[test]
    fn dest_matches_specific_requires_equal_orig_dest() {
        let want: IpAddr = "192.168.1.5".parse().unwrap();
        assert!(dest_matches(want, Some("192.168.1.5".parse().unwrap())));
        assert!(!dest_matches(want, Some("192.168.1.6".parse().unwrap())));
        // No recovered destination cannot satisfy a specific listener.
        assert!(!dest_matches(want, None));
    }

    #[tokio::test]
    async fn collect_reuses_one_collector_per_family_port() {
        let mgr = UdpManager::new();
        // A broadcast, a multicast, and a unicast destination that share a
        // port must all resolve to the SAME wildcard collector.
        let bcast = mgr
            .collect(&crate::test_reactor(), &ep("255.255.255.255:0"))
            .expect("bind bcast");
        let port = bcast.local_addr().unwrap().port();
        let unicast = mgr
            .collect(&crate::test_reactor(), &ep(&format!("192.168.1.5:{port}")))
            .expect("reuse unicast");
        let mcast = mgr
            .collect(&crate::test_reactor(), &ep(&format!("224.0.2.3:{port}")))
            .expect("reuse mcast");
        assert!(
            Arc::ptr_eq(&bcast.state, &unicast.state),
            "unicast dest on the same port must reuse the collector"
        );
        assert!(
            Arc::ptr_eq(&bcast.state, &mcast.state),
            "multicast dest on the same port must reuse the collector"
        );
    }

    #[tokio::test]
    async fn collector_binds_wildcard_never_the_destination() {
        let mgr = UdpManager::new();
        // Even a broadcast destination yields a wildcard (unspecified) bind.
        let c = mgr
            .collect(&crate::test_reactor(), &ep("255.255.255.255:0"))
            .expect("bind");
        assert!(
            c.bind_addr().ip().is_unspecified(),
            "collector must bind the wildcard, not the destination: {}",
            c.bind_addr()
        );
    }

    #[tokio::test]
    async fn multicast_listener_joins_group_unicast_does_not() {
        let mgr = UdpManager::new();
        let c = mgr
            .collect(&crate::test_reactor(), &ep("0.0.0.0:0"))
            .expect("bind");
        let _u = c.add_listener(&ep("0.0.0.0:0")).expect("wildcard listener");
        assert_eq!(
            c.joined_group_count(),
            0,
            "wildcard listener joins no group"
        );

        let _m = c
            .add_listener(&ep("224.0.2.3:0"))
            .expect("multicast listener");
        assert_eq!(
            c.joined_group_count(),
            1,
            "multicast listener must join exactly one group"
        );

        // A second listener on the same group must not re-join.
        let _m2 = c
            .add_listener(&ep("224.0.2.3:0"))
            .expect("second multicast listener");
        assert_eq!(
            c.joined_group_count(),
            1,
            "duplicate multicast group must be deduplicated"
        );
        assert_eq!(c.listener_count(), 3);
    }

    /// IPv4 sockets on UDP `port`, the collector's own included, from one
    /// sock_diag snapshot (`/proc/net/udp` repeats and skips rows under
    /// churn). Only Linux can say; elsewhere a timeout is never put down to a
    /// co-holder.
    fn udp_sockets_on(port: u16) -> usize {
        #[cfg(target_os = "linux")]
        {
            named_port::port_holders::udp_holders(port)
                .map(|h| h.iter().filter(|h| h.local.is_ipv4()).count())
                .unwrap_or(1)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = port;
            1
        }
    }

    /// What a stand-in does to the one datagram of an attempt.
    #[derive(Clone, Copy)]
    enum Fate {
        Deliver,
        /// A same-user, options-first socket co-binds the collector's number
        /// (a pvxs or softIoc process whose `bind(0)` was handed it) and takes
        /// the datagram, as the kernel's reuse hash does about half the time.
        #[cfg(target_os = "linux")]
        TakenByStranger,
        /// Sent to a number nobody listens on.
        #[cfg(target_os = "linux")]
        Misdirected,
    }

    impl Fate {
        /// The stranger co-bound on the collector's `port`, if any, and the
        /// number the datagram is sent to.
        fn stage(self, port: u16) -> (Option<std::net::UdpSocket>, u16) {
            match self {
                Fate::Deliver => (None, port),
                #[cfg(target_os = "linux")]
                Fate::TakenByStranger => {
                    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
                    s.set_reuse_address(true).unwrap();
                    s.set_reuse_port(true).unwrap();
                    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
                        .expect("co-bind the collector's number");
                    let s: std::net::UdpSocket = s.into();
                    s.set_nonblocking(true).unwrap();
                    (Some(s), port)
                }
                #[cfg(target_os = "linux")]
                Fate::Misdirected => {
                    // A number that is free now: bind, read, release.
                    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                    (None, probe.local_addr().unwrap().port())
                }
            }
        }
    }

    enum Attempt {
        Got(CollectedDatagram),
        /// Nothing arrived; `co_holders` other sockets sit on the number.
        TimedOut {
            co_holders: usize,
        },
    }

    async fn attempt(fate: Fate, wait: Duration) -> Attempt {
        let mgr = UdpManager::new();
        let collector = mgr
            .collect(&crate::test_reactor(), &ep("0.0.0.0:0"))
            .expect("bind wildcard collector");
        let port = collector.local_addr().unwrap().port();
        let mut rx = collector
            .add_listener(&ep("0.0.0.0:0"))
            .expect("wildcard listener");
        let (stranger, target) = fate.stage(port);
        // Send from fresh source ports until the stranger's share of the
        // reuse hash takes one, so the loss is not left to chance.
        let mut sent = 0;
        loop {
            let sender = UdpSocket::bind("127.0.0.1:0").await.expect("sender bind");
            sender
                .send_to(b"hello-pva", &format!("127.0.0.1:{target}"))
                .await
                .expect("send");
            sent += 1;
            let Some(stranger) = &stranger else { break };
            tokio::time::sleep(Duration::from_millis(10)).await;
            if stranger.recv(&mut [0u8; 64]).is_ok() {
                break;
            }
            // The collector got this one: drop it and draw another source.
            while rx.try_recv().is_ok() {}
            assert!(sent < 200, "the stranger never drew a datagram");
        }
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(got) => Attempt::Got(got.expect("channel open")),
            Err(_) => Attempt::TimedOut {
                co_holders: udp_sockets_on(port).saturating_sub(1),
            },
        }
    }

    /// The first datagram a wildcard collector delivers. A collector binds its
    /// ephemeral number before it sets the reuse options, so a stranger whose
    /// own `bind(0)` was handed that number co-binds it and takes some of the
    /// datagrams: a timeout with another socket on the number is the stranger,
    /// and a fresh collector is drawn. A timeout with nobody else there is the
    /// collector failing, and fails the test.
    async fn first_delivery(fates: &[Fate], wait: Duration) -> CollectedDatagram {
        const ATTEMPTS: usize = 4;
        let mut co_held = Vec::new();
        for n in 0..ATTEMPTS {
            let fate = fates.get(n).copied().unwrap_or(Fate::Deliver);
            match attempt(fate, wait).await {
                Attempt::Got(got) => return got,
                Attempt::TimedOut { co_holders: 0 } => {
                    panic!("datagram never arrived and no other socket holds the number")
                }
                Attempt::TimedOut { co_holders } => co_held.push(co_holders),
            }
        }
        panic!(
            "{ATTEMPTS} collectors in a row shared their number; co-holders per attempt: {co_held:?}"
        );
    }

    fn assert_recovered_orig_dest(got: &CollectedDatagram) {
        assert_eq!(&got.data, b"hello-pva");
        assert_eq!(
            got.orig_dest,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "original destination must be recovered from ancillary data"
        );
        assert_eq!(got.src.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    // Asserts a recovered original destination. Unix uses `recvmsg`/cmsg,
    // Windows uses `WSARecvMsg`/`IP_PKTINFO`; only other targets report
    // `orig_dest == None` by construction (see `crate::udp_rx`), so
    // the assertion is gated to the two that actually recover it. Binds a real
    // `tokio::net` UDP socket and drives `recv_loop`, whose spawn now lands on
    // the reactor-less callback pool under `exec_backend` (§4.2 UDP search is
    // deferred). Reactor-dependent — gated out on the exec backend (stage 3).
    #[cfg(any(unix, windows))]
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wildcard_listener_receives_with_orig_dest() {
        // Send a unicast datagram to loopback; the collector must recover
        // 127.0.0.1 as the original destination via the pktinfo cmsg.
        let got = first_delivery(&[], Duration::from_secs(3)).await;
        assert_recovered_orig_dest(&got);
    }

    #[cfg(target_os = "linux")]
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stranger_on_the_collectors_number_costs_a_fresh_collector_not_the_test() {
        let got = first_delivery(&[Fate::TakenByStranger], Duration::from_secs(1)).await;
        assert_recovered_orig_dest(&got);
    }

    #[cfg(target_os = "linux")]
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[should_panic(expected = "no other socket holds the number")]
    async fn a_collector_that_never_delivers_still_fails_the_test() {
        first_delivery(&[Fate::Misdirected], Duration::from_millis(500)).await;
    }

    /// A port-0 collector binds before its fanout options, so it never lands
    /// on a number a same-user fanout socket holds and loses its datagrams to
    /// that socket's reuse group (`epics_base_rs::net::bind_fanout`).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_port_0_collector_never_takes_a_strangers_number() {
        let mut theirs = HashSet::new();
        let mut held = Vec::new();
        for _ in 0..2000 {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
            s.set_reuse_address(true).unwrap();
            s.set_reuse_port(true).unwrap();
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())
                .unwrap();
            theirs.insert(s.local_addr().unwrap().as_socket().unwrap().port());
            held.push(s);
        }
        let mgr = UdpManager::new();
        for _ in 0..200 {
            let c = mgr
                .collect(&crate::test_reactor(), &ep("0.0.0.0:0"))
                .expect("bind collector");
            let port = c.local_addr().unwrap().port();
            assert!(!theirs.contains(&port), "collector on {port}, a stranger's");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn specific_listener_filters_by_orig_dest() {
        let mgr = UdpManager::new();
        let collector = mgr
            .collect(&crate::test_reactor(), &ep("0.0.0.0:0"))
            .expect("bind collector");
        let port = collector.local_addr().unwrap().port();
        // Listener wants datagrams destined to 10.255.255.255 — loopback
        // traffic must NOT reach it.
        let mut rx = collector
            .add_listener(&ep("10.255.255.255:0"))
            .expect("specific listener");

        let sender = UdpSocket::bind("127.0.0.1:0").await.expect("sender bind");
        sender
            .send_to(b"hello", &format!("127.0.0.1:{port}"))
            .await
            .expect("send");

        let got = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            got.is_err(),
            "a listener bound to a different destination must not receive loopback traffic"
        );
    }
}
