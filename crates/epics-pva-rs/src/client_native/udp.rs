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

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

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
        match (group, dest.addr) {
            (IpAddr::V4(g), SocketAddr::V4(_)) => {
                // `@iface` selects one interface; modifier-less joins on the
                // unspecified interface and lets the OS choose (pvxs resolves
                // the endpoint's interface, defaulting to wildcard).
                let iface = match dest.iface.as_deref() {
                    Some(spec) => crate::config::env::resolve_iface_v4(spec)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                    None => Ipv4Addr::UNSPECIFIED,
                };
                self.state.sock.join_multicast_v4(g, iface)?;
            }
            (IpAddr::V6(g), SocketAddr::V6(_)) => {
                // v6 multicast joins by scope id; 0 = let the OS pick.
                self.state.sock.join_multicast_v6(&g, 0)?;
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
    pub fn collect(&self, dest: &Endpoint) -> io::Result<UdpCollector> {
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

        let state = CollectorState::bind_and_spawn(is_v6, port)?;
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
    fn bind_and_spawn(is_v6: bool, port: u16) -> io::Result<Arc<CollectorState>> {
        let domain = if is_v6 { Domain::IPV6 } else { Domain::IPV4 };
        let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        // `epicsSocketEnableAddressUseForDatagramFanout` (pvxs
        // `udp_collector.cpp:136`): SO_REUSEADDR (+ SO_REUSEPORT on unix) so
        // this wildcard listener can coexist with a co-located server's UDP
        // responder and with sibling collectors on the same port.
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;

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
        enable_recv_orig_dest(&sock, is_v6)?;

        let bind_addr: SocketAddr = if is_v6 {
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
        } else {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
        };
        sock.bind(&bind_addr.into())?;

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
        tokio::spawn(recv_loop(sock, is_v6, Arc::downgrade(&state)));

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
async fn recv_loop(sock: Arc<UdpSocket>, is_v6: bool, weak: Weak<CollectorState>) {
    // PVA UDP messages fit well under 64 KiB; one reusable buffer.
    let mut buf = vec![0u8; 0x1_0000];
    loop {
        // Wake on readability, but bound the wait so an idle collector still
        // notices that its last handle was dropped and can exit.
        match tokio::time::timeout(Duration::from_secs(5), sock.readable()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break,
            Err(_elapsed) => {
                if weak.strong_count() == 0 {
                    break;
                }
                continue;
            }
        }

        match recv_with_orig_dest(&sock, is_v6, &mut buf) {
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

/// Enable the socket option that makes the kernel attach each datagram's
/// original destination address as ancillary data.
///
/// * v4 Linux: `IP_PKTINFO` (yields `in_pktinfo.ipi_addr`).
/// * v4 macOS/BSD: `IP_RECVDSTADDR` (yields a bare `in_addr`).
/// * v6: `IPV6_RECVPKTINFO` (yields `in6_pktinfo.ipi6_addr`).
fn enable_recv_orig_dest(sock: &Socket, is_v6: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let (level, optname) = if is_v6 {
        (libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO)
    } else {
        #[cfg(target_os = "linux")]
        {
            (libc::IPPROTO_IP, libc::IP_PKTINFO)
        }
        #[cfg(not(target_os = "linux"))]
        {
            (libc::IPPROTO_IP, libc::IP_RECVDSTADDR)
        }
    };

    let on: libc::c_int = 1;
    // SAFETY: `fd` is owned by `sock` for the duration of this call; the
    // option value outlives the syscall.
    let r = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            level,
            optname,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drive one non-blocking `recvmsg`, returning the byte count, source
/// address, and recovered original destination. `Ok(None)` means the read
/// would block (re-arm and wait).
fn recv_with_orig_dest(
    sock: &UdpSocket,
    is_v6: bool,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, Option<IpAddr>)>> {
    let res = sock.try_io(tokio::io::Interest::READABLE, || {
        // SAFETY: every pointer below refers to a local that outlives the
        // single `recvmsg` call; the destination buffers are borrowed for
        // the duration of the call.
        unsafe {
            let mut storage: libc::sockaddr_storage = std::mem::zeroed();
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            };
            let mut cbuf = [0u8; 128];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_name = &mut storage as *mut _ as *mut libc::c_void;
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cbuf.len() as _;

            let n = libc::recvmsg(std::os::fd::AsRawFd::as_raw_fd(sock), &mut msg, 0);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }

            let src = sockaddr_storage_to_socketaddr(&storage, msg.msg_namelen)?;
            let orig_dest = orig_dest_from_cmsgs(&msg, is_v6);
            Ok((n as usize, src, orig_dest))
        }
    });

    match res {
        Ok(v) => Ok(Some(v)),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

/// Walk the ancillary-data list for the original-destination cmsg and
/// decode it. Returns `None` when no matching cmsg is present.
///
/// # Safety
/// `msg.msg_control` must point at `msg.msg_controllen` valid bytes
/// populated by a preceding `recvmsg`.
unsafe fn orig_dest_from_cmsgs(msg: &libc::msghdr, is_v6: bool) -> Option<IpAddr> {
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        let hdr = unsafe { &*cmsg };
        if let Some(ip) = unsafe { decode_orig_dest_cmsg(hdr, cmsg, is_v6) } {
            return Some(ip);
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    None
}

/// Decode a single cmsg into an original-destination IP if it carries one.
///
/// # Safety
/// `cmsg_ptr` must equal `&*hdr` and reference a cmsg whose payload was
/// populated by `recvmsg`.
unsafe fn decode_orig_dest_cmsg(
    hdr: &libc::cmsghdr,
    cmsg_ptr: *const libc::cmsghdr,
    is_v6: bool,
) -> Option<IpAddr> {
    let fits =
        |size: usize| hdr.cmsg_len as usize >= unsafe { libc::CMSG_LEN(size as u32) } as usize;

    if is_v6 {
        if hdr.cmsg_level == libc::IPPROTO_IPV6
            && hdr.cmsg_type == libc::IPV6_PKTINFO
            && fits(std::mem::size_of::<libc::in6_pktinfo>())
        {
            let info = unsafe {
                std::ptr::read_unaligned(libc::CMSG_DATA(cmsg_ptr) as *const libc::in6_pktinfo)
            };
            return Some(IpAddr::V6(Ipv6Addr::from(info.ipi6_addr.s6_addr)));
        }
        return None;
    }

    // v4 Linux: IP_PKTINFO carries in_pktinfo with the destination in ipi_addr.
    #[cfg(target_os = "linux")]
    if hdr.cmsg_level == libc::IPPROTO_IP
        && hdr.cmsg_type == libc::IP_PKTINFO
        && fits(std::mem::size_of::<libc::in_pktinfo>())
    {
        let info = unsafe {
            std::ptr::read_unaligned(libc::CMSG_DATA(cmsg_ptr) as *const libc::in_pktinfo)
        };
        return Some(IpAddr::V4(Ipv4Addr::from(
            info.ipi_addr.s_addr.to_ne_bytes(),
        )));
    }

    // v4 macOS/BSD: IP_RECVDSTADDR carries a bare in_addr (the destination).
    #[cfg(not(target_os = "linux"))]
    if hdr.cmsg_level == libc::IPPROTO_IP
        && hdr.cmsg_type == libc::IP_RECVDSTADDR
        && fits(std::mem::size_of::<libc::in_addr>())
    {
        let addr =
            unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(cmsg_ptr) as *const libc::in_addr) };
        return Some(IpAddr::V4(Ipv4Addr::from(addr.s_addr.to_ne_bytes())));
    }

    None
}

/// Convert a kernel-filled `sockaddr_storage` into a [`SocketAddr`].
///
/// # Safety
/// `storage` must be initialised by `recvmsg` and `len` is its
/// `msg_namelen`.
unsafe fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
    _len: libc::socklen_t,
) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("recvmsg returned unsupported address family {other}"),
        )),
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
        let bcast = mgr.collect(&ep("255.255.255.255:0")).expect("bind bcast");
        let port = bcast.local_addr().unwrap().port();
        let unicast = mgr
            .collect(&ep(&format!("192.168.1.5:{port}")))
            .expect("reuse unicast");
        let mcast = mgr
            .collect(&ep(&format!("224.0.2.3:{port}")))
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
        let c = mgr.collect(&ep("255.255.255.255:0")).expect("bind");
        assert!(
            c.bind_addr().ip().is_unspecified(),
            "collector must bind the wildcard, not the destination: {}",
            c.bind_addr()
        );
    }

    #[tokio::test]
    async fn multicast_listener_joins_group_unicast_does_not() {
        let mgr = UdpManager::new();
        let c = mgr.collect(&ep("0.0.0.0:0")).expect("bind");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wildcard_listener_receives_with_orig_dest() {
        let mgr = UdpManager::new();
        let collector = mgr
            .collect(&ep("0.0.0.0:0"))
            .expect("bind wildcard collector");
        let port = collector.local_addr().unwrap().port();
        let mut rx = collector
            .add_listener(&ep("0.0.0.0:0"))
            .expect("wildcard listener");

        // Send a unicast datagram to loopback; the collector must recover
        // 127.0.0.1 as the original destination via the pktinfo cmsg.
        let sender = UdpSocket::bind("127.0.0.1:0").await.expect("sender bind");
        sender
            .send_to(b"hello-pva", &format!("127.0.0.1:{port}"))
            .await
            .expect("send");

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("datagram must arrive")
            .expect("channel open");
        assert_eq!(&got.data, b"hello-pva");
        assert_eq!(
            got.orig_dest,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "original destination must be recovered from ancillary data"
        );
        assert_eq!(got.src.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn specific_listener_filters_by_orig_dest() {
        let mgr = UdpManager::new();
        let collector = mgr.collect(&ep("0.0.0.0:0")).expect("bind collector");
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
