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

// RTEMS-EXEC-MODEL-ALLOW(4): this file does not build on the exec backend at
// all — `client_native/mod.rs` declares it `#[cfg(tokio_backend)]` because its
// `UdpSocket::readable` waits need a reactor the exec backend has not got. The
// four are counted, not run.
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
        reactor.spawn(recv_loop(sock, is_v6, Arc::downgrade(&state)));

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
/// * v4 Linux family (`linux`, `android`): `IP_PKTINFO` (yields
///   `in_pktinfo.ipi_addr`). These share the Linux IP stack but carry
///   distinct `target_os` values, and `libc` defines `IP_RECVDSTADDR`
///   for neither — gating on `target_os = "linux"` alone would fail to
///   compile on `android`.
/// * v4 BSD/Apple family — `macos`, `netbsd`, and the *BSDs that do NOT
///   provide `IP_PKTINFO` (`freebsd`, `openbsd`, `dragonfly`):
///   `IP_RECVDSTADDR` (yields a bare `in_addr`). It is the only v4
///   option `libc` defines across the whole BSD family, so it is
///   preferred over pvxs's `IP_ORIGDSTADDR` (freebsd-only in `libc`;
///   absent on `openbsd`/`netbsd`).
/// * v6: `IPV6_RECVPKTINFO` (yields `in6_pktinfo.ipi6_addr`).
///
/// The `optname` selected here MUST equal the `cmsg_type` matched in
/// [`decode_orig_dest_cmsg`]; keep the two cfg groups identical.
#[cfg(unix)]
fn enable_recv_orig_dest(sock: &Socket, is_v6: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let (level, optname) = if is_v6 {
        (libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO)
    } else {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            (libc::IPPROTO_IP, libc::IP_PKTINFO)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
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

/// Windows: enable `IP_PKTINFO` / `IPV6_PKTINFO` so each datagram's
/// destination arrives as ancillary data, and resolve the `WSARecvMsg`
/// extension-fn pointer up front so the hot recv path can't fail on first
/// use (mirrors pvxs `enable_IP_PKTINFO` + `oseDoOnce`,
/// `os/WIN32/osdSockExt.cpp`). Unlike Unix, Windows recovers the v4
/// destination from `IP_PKTINFO` too — there is no `IP_RECVDSTADDR`.
#[cfg(windows)]
fn enable_recv_orig_dest(sock: &Socket, is_v6: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        IP_PKTINFO, IPPROTO_IP, IPPROTO_IPV6, IPV6_PKTINFO, SOCKET, setsockopt,
    };

    let raw = sock.as_raw_socket() as SOCKET;
    let on: i32 = 1;
    let (level, optname) = if is_v6 {
        (IPPROTO_IPV6 as i32, IPV6_PKTINFO as i32)
    } else {
        (IPPROTO_IP as i32, IP_PKTINFO as i32)
    };
    // SAFETY: `raw` is owned by `sock` for this call; `on` outlives it.
    let r = unsafe {
        setsockopt(
            raw,
            level,
            optname,
            &on as *const i32 as *const u8,
            std::mem::size_of_val(&on) as i32,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(win_orig_dest::last_wsa_error()));
    }
    // Resolve+cache the WSARecvMsg extension pointer while we hold a socket.
    win_orig_dest::resolve_wsarecvmsg(raw)?;
    Ok(())
}

/// Other non-Unix, non-Windows targets: no ancillary-data mechanism, so
/// the collector receives without original-destination recovery
/// (`orig_dest == None`). See [`recv_with_orig_dest`].
#[cfg(all(not(unix), not(windows)))]
fn enable_recv_orig_dest(_sock: &Socket, _is_v6: bool) -> io::Result<()> {
    Ok(())
}

/// Drive one non-blocking `recvmsg`, returning the byte count, source
/// address, and recovered original destination. `Ok(None)` means the read
/// would block (re-arm and wait).
#[cfg(unix)]
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

/// Windows: one non-blocking `WSARecvMsg`, recovering the original
/// destination from the `IP_PKTINFO` / `IPV6_PKTINFO` ancillary data.
/// `Ok(None)` on `WSAEWOULDBLOCK` mirrors the Unix re-arm contract; the
/// `try_io` wrapper clears tokio's readiness on that.
#[cfg(windows)]
fn recv_with_orig_dest(
    sock: &UdpSocket,
    is_v6: bool,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, Option<IpAddr>)>> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::SOCKET;

    let raw = sock.as_raw_socket() as SOCKET;
    let res = sock.try_io(tokio::io::Interest::READABLE, || {
        // SAFETY: every pointer built inside refers to a local that
        // outlives the single WSARecvMsg call; `buf` is borrowed for it.
        unsafe { win_orig_dest::recvmsg_once(raw, is_v6, buf) }
    });

    match res {
        Ok(v) => Ok(Some(v)),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

/// Other non-Unix, non-Windows targets: receive without ancillary data.
/// Yields the source address and `None` for the original destination.
/// `try_recv_from` mirrors the Unix `WouldBlock` -> `Ok(None)` contract.
#[cfg(all(not(unix), not(windows)))]
fn recv_with_orig_dest(
    sock: &UdpSocket,
    _is_v6: bool,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, Option<IpAddr>)>> {
    match sock.try_recv_from(buf) {
        Ok((n, src)) => Ok(Some((n, src, None))),
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
#[cfg(unix)]
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
#[cfg(unix)]
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

    // v4 Linux family (linux, android): IP_PKTINFO carries in_pktinfo with
    // the destination in ipi_addr. Mirrors `enable_recv_orig_dest`'s cfg.
    #[cfg(any(target_os = "linux", target_os = "android"))]
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

    // v4 BSD/Apple family (macos, netbsd, and the IP_PKTINFO-less freebsd/
    // openbsd/dragonfly): IP_RECVDSTADDR carries a bare in_addr (the
    // destination). Mirrors `enable_recv_orig_dest`'s cfg.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
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
#[cfg(unix)]
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

/// Windows original-destination recovery via `WSARecvMsg` + `IP_PKTINFO`,
/// the Winsock analog of the Unix `recvmsg`/cmsg path. Mirrors pvxs
/// `os/WIN32/osdSockExt.cpp` (`recvfromx::call`). Scoped to 64-bit Windows
/// targets (the matrix builds x86_64 + aarch64), where the cmsg header and
/// data alignments both equal `align_of::<CMSGHDR>()`.
#[cfg(windows)]
mod win_orig_dest {
    use super::*;
    use core::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{null, null_mut, read_unaligned};
    use std::sync::OnceLock;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, CMSGHDR, IN_PKTINFO, IN6_PKTINFO, IP_PKTINFO, IPPROTO_IP, IPPROTO_IPV6,
        IPV6_PKTINFO, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKADDR_STORAGE, SOCKET, WSABUF, WSAGetLastError, WSAID_WSARECVMSG, WSAIoctl, WSAMSG,
    };
    use windows_sys::core::GUID;

    /// `WSARecvMsg` extension fn. Its trailing `OVERLAPPED*` and completion
    /// routine are always null here, so they are typed as opaque pointers to
    /// avoid naming extra types.
    type WsaRecvMsgFn =
        unsafe extern "system" fn(SOCKET, *mut WSAMSG, *mut u32, *mut c_void, *mut c_void) -> i32;

    static WSARECVMSG: OnceLock<WsaRecvMsgFn> = OnceLock::new();

    pub(super) fn last_wsa_error() -> i32 {
        // SAFETY: `WSAGetLastError` has no preconditions.
        unsafe { WSAGetLastError() }
    }

    /// Resolve and cache the `WSARecvMsg` extension-fn pointer via
    /// `WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER)` (pvxs `oseDoOnce`).
    pub(super) fn resolve_wsarecvmsg(sock: SOCKET) -> io::Result<WsaRecvMsgFn> {
        if let Some(f) = WSARECVMSG.get() {
            return Ok(*f);
        }
        let guid: GUID = WSAID_WSARECVMSG;
        let mut func: Option<WsaRecvMsgFn> = None;
        let mut nout: u32 = 0;
        // SAFETY: `guid`/`func`/`nout` outlive the call; the sizes match.
        let rc = unsafe {
            WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &guid as *const GUID as *const c_void,
                size_of::<GUID>() as u32,
                &mut func as *mut Option<WsaRecvMsgFn> as *mut c_void,
                size_of::<Option<WsaRecvMsgFn>>() as u32,
                &mut nout,
                null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(last_wsa_error()));
        }
        let func = func.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "WSARecvMsg pointer unavailable")
        })?;
        let _ = WSARECVMSG.set(func);
        Ok(func)
    }

    /// One non-blocking `WSARecvMsg`.
    ///
    /// # Safety
    /// `buf` must be writable for its full length; the call borrows it.
    pub(super) unsafe fn recvmsg_once(
        sock: SOCKET,
        is_v6: bool,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, Option<IpAddr>)> {
        let recv = resolve_wsarecvmsg(sock)?;

        // SAFETY: zeroed sockaddr/WSAMSG are valid initial states; every
        // pointer below refers to a stack local that outlives the call.
        let mut storage: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut iov = WSABUF {
            len: buf.len() as u32,
            buf: buf.as_mut_ptr(),
        };
        // Room for one in6_pktinfo cmsg (header + 20 bytes, aligned).
        let mut cbuf = [0u8; 128];
        let mut msg: WSAMSG = unsafe { std::mem::zeroed() };
        msg.name = &mut storage as *mut SOCKADDR_STORAGE as *mut _;
        msg.namelen = size_of::<SOCKADDR_STORAGE>() as i32;
        msg.lpBuffers = &mut iov;
        msg.dwBufferCount = 1;
        msg.Control = WSABUF {
            len: cbuf.len() as u32,
            buf: cbuf.as_mut_ptr(),
        };
        msg.dwFlags = 0;

        let mut nrx: u32 = 0;
        // SAFETY: `recv` is the resolved WSARecvMsg; pointers outlive it.
        let rc = unsafe { recv(sock, &mut msg, &mut nrx, null_mut(), null_mut()) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(last_wsa_error()));
        }

        let src = sockaddr_to_socketaddr(&storage)?;
        // SAFETY: `msg.Control` was populated by the WSARecvMsg above.
        let orig_dest = unsafe { orig_dest_from_wsacmsgs(&msg, is_v6) };
        Ok((nrx as usize, src, orig_dest))
    }

    #[inline]
    fn cmsg_align(len: usize) -> usize {
        let a = std::mem::align_of::<CMSGHDR>();
        (len + a - 1) & !(a - 1)
    }

    /// `WSA_CMSG_DATA`: payload pointer for a cmsg header.
    ///
    /// # Safety
    /// `cmsg` must point at a valid `CMSGHDR` inside the control buffer.
    unsafe fn wsa_cmsg_data(cmsg: *const CMSGHDR) -> *const u8 {
        (cmsg as *const u8).wrapping_add(cmsg_align(size_of::<CMSGHDR>()))
    }

    /// `WSA_CMSG_FIRSTHDR`.
    fn wsa_cmsg_firsthdr(msg: &WSAMSG) -> *const CMSGHDR {
        if (msg.Control.len as usize) >= size_of::<CMSGHDR>() {
            msg.Control.buf as *const CMSGHDR
        } else {
            null()
        }
    }

    /// `WSA_CMSG_NXTHDR`. Bounds the walk by `Control.len` using integer
    /// arithmetic so no out-of-range pointer is ever formed.
    ///
    /// # Safety
    /// `cmsg`, when non-null, must point at a valid `CMSGHDR`.
    unsafe fn wsa_cmsg_nxthdr(msg: &WSAMSG, cmsg: *const CMSGHDR) -> *const CMSGHDR {
        if cmsg.is_null() {
            return wsa_cmsg_firsthdr(msg);
        }
        // SAFETY: caller guarantees `cmsg` is a valid header.
        let cmsg_len = unsafe { (*cmsg).cmsg_len } as usize;
        let base = msg.Control.buf as usize;
        let end = base + msg.Control.len as usize;
        let next = (cmsg as usize) + cmsg_align(cmsg_len);
        if next + size_of::<CMSGHDR>() > end {
            null()
        } else {
            next as *const CMSGHDR
        }
    }

    /// Walk the ancillary data for the `IP_PKTINFO` / `IPV6_PKTINFO` cmsg
    /// and return the recovered destination IP, or `None`.
    ///
    /// # Safety
    /// `msg.Control` must point at `Control.len` valid bytes from `WSARecvMsg`.
    unsafe fn orig_dest_from_wsacmsgs(msg: &WSAMSG, is_v6: bool) -> Option<IpAddr> {
        let mut cmsg = wsa_cmsg_firsthdr(msg);
        while !cmsg.is_null() {
            // SAFETY: `cmsg` is non-null and within the control buffer.
            let hdr = unsafe { &*cmsg };
            if is_v6 {
                if hdr.cmsg_level == IPPROTO_IPV6 as i32 && hdr.cmsg_type == IPV6_PKTINFO as i32 {
                    // SAFETY: an IPV6_PKTINFO cmsg payload is an in6_pktinfo.
                    let info = unsafe { read_unaligned(wsa_cmsg_data(cmsg) as *const IN6_PKTINFO) };
                    return Some(IpAddr::V6(Ipv6Addr::from(unsafe { info.ipi6_addr.u.Byte })));
                }
            } else if hdr.cmsg_level == IPPROTO_IP as i32 && hdr.cmsg_type == IP_PKTINFO as i32 {
                // SAFETY: an IP_PKTINFO cmsg payload is an in_pktinfo.
                let info = unsafe { read_unaligned(wsa_cmsg_data(cmsg) as *const IN_PKTINFO) };
                return Some(IpAddr::V4(Ipv4Addr::from(unsafe {
                    info.ipi_addr.S_un.S_addr.to_ne_bytes()
                })));
            }
            // SAFETY: `cmsg` is a valid header from this same control buffer.
            cmsg = unsafe { wsa_cmsg_nxthdr(msg, cmsg) };
        }
        None
    }

    /// Convert a `WSARecvMsg`-filled `SOCKADDR_STORAGE` into a [`SocketAddr`].
    fn sockaddr_to_socketaddr(storage: &SOCKADDR_STORAGE) -> io::Result<SocketAddr> {
        match storage.ss_family {
            AF_INET => {
                // SAFETY: ss_family == AF_INET ⟹ storage is a SOCKADDR_IN.
                let sin = unsafe { &*(storage as *const SOCKADDR_STORAGE as *const SOCKADDR_IN) };
                let ip = Ipv4Addr::from(unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes());
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    ip,
                    u16::from_be(sin.sin_port),
                )))
            }
            AF_INET6 => {
                // SAFETY: ss_family == AF_INET6 ⟹ storage is a SOCKADDR_IN6.
                let sin6 = unsafe { &*(storage as *const SOCKADDR_STORAGE as *const SOCKADDR_IN6) };
                let ip = Ipv6Addr::from(unsafe { sin6.sin6_addr.u.Byte });
                let scope = unsafe { sin6.Anonymous.sin6_scope_id };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    u16::from_be(sin6.sin6_port),
                    sin6.sin6_flowinfo,
                    scope,
                )))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WSARecvMsg returned unsupported address family {other}"),
            )),
        }
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

    // Asserts a recovered original destination. Unix uses `recvmsg`/cmsg,
    // Windows uses `WSARecvMsg`/`IP_PKTINFO`; only other targets report
    // `orig_dest == None` by construction (see `enable_recv_orig_dest`), so
    // the assertion is gated to the two that actually recover it. Binds a real
    // `tokio::net` UDP socket and drives `recv_loop`, whose spawn now lands on
    // the reactor-less callback pool under `exec_backend` (§4.2 UDP search is
    // deferred). Reactor-dependent — gated out on the exec backend (stage 3).
    #[cfg(any(unix, windows))]
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wildcard_listener_receives_with_orig_dest() {
        let mgr = UdpManager::new();
        let collector = mgr
            .collect(&crate::test_reactor(), &ep("0.0.0.0:0"))
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
