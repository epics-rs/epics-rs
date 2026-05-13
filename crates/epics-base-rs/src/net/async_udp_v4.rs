//! Cross-platform per-NIC async IPv4 UDP socket (libca convention).
//!
//! # Strategy
//!
//! Plain `tokio::net::UdpSocket` bound to `0.0.0.0` lets the OS routing
//! table pick the egress NIC for outgoing packets. On a multi-NIC host
//! that means `255.255.255.255` and multicast traffic only leaves via
//! the default-route interface — IOCs reachable only via the secondary
//! NIC never see the SEARCH burst.
//!
//! libca solves this with `osiSockDiscoverInterfaces`: one socket per
//! up, non-loopback IPv4 interface, each `bind`ed to that interface's
//! IP. The kernel routes outbound traffic according to the source IP,
//! so each socket forces packets out its own NIC. Inbound traffic
//! addressed to the NIC's IP / subnet broadcast / `255.255.255.255`
//! lands on the matching socket; we multiplex all sockets on receive.
//!
//! # API
//!
//! * [`AsyncUdpV4::bind`] — enumerate interfaces, create one
//!   `tokio::net::UdpSocket` per up-non-loopback NIC + a loopback
//!   socket. Configures `SO_REUSEADDR`, optional `SO_BROADCAST`, and
//!   on Linux `IP_MULTICAST_ALL=0`.
//! * [`AsyncUdpV4::send_to`] — pick the NIC whose subnet contains
//!   `dest` (or fall back to a default).
//! * [`AsyncUdpV4::send_via`] — explicit per-NIC send, by interface
//!   IP. Used by SEARCH responders to reply via the same NIC the
//!   request arrived on.
//! * [`AsyncUdpV4::fanout_to`] — send the same payload via every NIC.
//!   For `255.255.255.255` and `IPv4` multicast destinations.
//! * [`AsyncUdpV4::recv_with_meta`] — receive on whichever socket
//!   becomes ready first. Synthesises [`RecvMeta::ifindex`] and
//!   [`RecvMeta::dst_ip`] from the receiving socket's known NIC info.
//!
//! Each NIC's socket binds to a *separate* ephemeral port when
//! `port = 0`. Use [`AsyncUdpV4::ifaces`] to inspect the resulting
//! socket-per-NIC mapping (e.g. for diagnostics or NS-driven response
//! correlation).

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use super::iface_map::{IfaceInfo, IfaceMap};

/// Metadata returned by [`AsyncUdpV4::recv_with_meta`].
#[derive(Debug, Clone, Copy)]
pub struct RecvMeta {
    /// Number of bytes written into the caller's buffer.
    pub n: usize,
    /// Source address as seen on the wire.
    pub src: SocketAddr,
    /// Destination IP — synthesized from the receiving socket's NIC IP.
    /// `None` only if the receiving socket was bound to the wildcard.
    pub dst_ip: Option<Ipv4Addr>,
    /// Receiving interface index (kernel ifindex). `None` only if the
    /// platform did not surface an index for this interface.
    pub ifindex: Option<u32>,
    /// IP address of the NIC that received the packet.
    pub iface_ip: Ipv4Addr,
}

/// One bound per-NIC socket plus its NIC metadata.
pub struct NicSocket {
    pub sock: Arc<UdpSocket>,
    /// NIC's unicast IPv4 address (used for routing replies and
    /// per-NIC sends). For an [`Self::rx_only_bcast`] socket this is
    /// still the underlying NIC's unicast IP, NOT the broadcast it's
    /// bound to.
    pub iface_ip: Ipv4Addr,
    /// Kernel interface index (0 = unknown, treat as sentinel).
    pub ifindex: u32,
    /// IPv4 netmask for routing decisions.
    pub netmask: Ipv4Addr,
    /// Subnet broadcast address (e.g. `10.0.0.255`), if reported.
    pub broadcast: Option<Ipv4Addr>,
    /// Whether this is the loopback NIC.
    pub is_loopback: bool,
    /// True for an auxiliary socket bound to the NIC's broadcast
    /// address solely to receive subnet broadcasts (BSD/macOS oddity:
    /// a socket bound to the NIC's unicast IP does NOT receive
    /// packets sent to the subnet broadcast address — see EPICS-base
    /// `rsrv/caservertask.c:670-708`). Send paths skip these sockets.
    pub rx_only_bcast: bool,
}

impl std::fmt::Debug for NicSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NicSocket")
            .field("iface_ip", &self.iface_ip)
            .field("ifindex", &self.ifindex)
            .field("netmask", &self.netmask)
            .field("broadcast", &self.broadcast)
            .field("is_loopback", &self.is_loopback)
            .field("rx_only_bcast", &self.rx_only_bcast)
            .field(
                "local_addr",
                &self
                    .sock
                    .local_addr()
                    .ok()
                    .unwrap_or_else(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))),
            )
            .finish()
    }
}

/// Per-NIC UDP socket bundle. See module docs.
pub struct AsyncUdpV4 {
    sockets: Vec<NicSocket>,
}

impl AsyncUdpV4 {
    /// Bind one socket per IPv4 interface (incl. loopback) on `port`.
    /// Use `port = 0` for an ephemeral port — each NIC picks its own.
    ///
    /// `broadcast=true` enables `SO_BROADCAST` (required for any
    /// `255.255.255.255` or per-subnet broadcast send).
    ///
    /// Returns an error only when *every* attempted bind fails. A
    /// single-NIC failure is logged at `debug` and skipped — partial
    /// fanout is preferable to a hard error in transient
    /// interface-flapping scenarios.
    pub fn bind(port: u16, broadcast: bool) -> io::Result<Self> {
        Self::bind_with_map(&IfaceMap::new(), port, broadcast)
    }

    /// Like [`Self::bind`] but skips the loopback NIC. Use this when
    /// the bound port is shared across processes (e.g. PVA UDP 5076)
    /// and a co-bound loopback socket on another local process would
    /// race with this one for incoming packets via SO_REUSEPORT
    /// load-balancing on macOS / Linux.
    ///
    /// Concrete failure this avoids: a pva-rs *client* binding its
    /// beacon-receive socket on `127.0.0.1:5076` while a pva-rs
    /// *server* on the same host has its UDP responder bound on
    /// `127.0.0.1:5076`. macOS distributes inbound `127.0.0.1:5076`
    /// packets between them via SO_REUSEPORT, so SEARCH packets
    /// destined to the server randomly land on the client's beacon
    /// socket and are silently dropped (the beacon receiver only
    /// processes BEACON, not SEARCH).
    pub fn bind_non_loopback(port: u16, broadcast: bool) -> io::Result<Self> {
        Self::bind_with_map_filtered(&IfaceMap::new(), port, broadcast, true)
    }

    /// Like [`Self::bind`] but reuses an existing [`IfaceMap`] —
    /// useful when callers maintain a long-lived shared map.
    pub fn bind_with_map(map: &IfaceMap, port: u16, broadcast: bool) -> io::Result<Self> {
        Self::bind_with_map_filtered(map, port, broadcast, false)
    }

    fn bind_with_map_filtered(
        map: &IfaceMap,
        port: u16,
        broadcast: bool,
        skip_loopback: bool,
    ) -> io::Result<Self> {
        let ifaces = map.all();
        let mut sockets = Vec::with_capacity(ifaces.len() * 2);
        for info in ifaces {
            if skip_loopback && info.ip.is_loopback() {
                continue;
            }
            match bind_one(&info, port, broadcast) {
                Ok(nic) => sockets.push(nic),
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface = %info.ip,
                        port,
                        error = %e,
                        "skipping NIC: bind failed"
                    );
                }
            }
            // BSD-family oddity (macOS, *BSD): a UDP socket bound to a
            // specific NIC unicast IP receives only unicasts to that
            // IP — packets sent to the subnet broadcast address are
            // delivered ONLY to a socket bound to either the broadcast
            // address itself or to INADDR_ANY. Mirror EPICS-base rsrv
            // (`caservertask.c:670-708`) and bind a second RX-only
            // socket to each NIC's broadcast address so PVA/CA SEARCH
            // bursts sent to e.g. `192.168.1.255:5076` reach the
            // responder. Windows: Winsock delivers subnet broadcasts
            // to the unicast-bound socket, so the extra bind is
            // unnecessary and skipped. Loopback has no broadcast.
            // Per-NIC failures are logged at debug; the unicast
            // socket above is the load-bearing one.
            #[cfg(not(target_os = "windows"))]
            if let Some(bcast) = info.broadcast {
                if !info.ip.is_loopback() && !bcast.is_unspecified() {
                    match bind_one_at(&info, bcast, port, broadcast, true) {
                        Ok(nic) => sockets.push(nic),
                        Err(e) => {
                            tracing::debug!(
                                target: "epics_base_rs::net",
                                iface = %info.ip,
                                bcast = %bcast,
                                port,
                                error = %e,
                                "skipping NIC bcast bind"
                            );
                        }
                    }
                }
            }
        }
        if sockets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "AsyncUdpV4: failed to bind any interface",
            ));
        }
        Ok(Self { sockets })
    }

    /// Like [`Self::bind`] but every per-NIC socket binds to the *same*
    /// ephemeral port. The first up-non-loopback NIC picks the port
    /// (kernel-assigned via `port=0`); remaining NICs reuse it via
    /// `SO_REUSEADDR` (allowed because each socket binds a different
    /// IP). NICs that fail to bind to the chosen port are logged at
    /// `debug` and skipped.
    ///
    /// This is the right choice for protocols that embed the local
    /// reply port inside outgoing packets (PVA SEARCH, CA repeater
    /// register) — every NIC's reply port is identical, so an IOC
    /// replying to the source IP+port reaches the same logical socket
    /// regardless of which NIC it came back through.
    pub fn bind_ephemeral_same_port(broadcast: bool) -> io::Result<Self> {
        Self::bind_ephemeral_same_port_with_map(&IfaceMap::new(), broadcast)
    }

    /// Like [`Self::bind_ephemeral_same_port`] but reuses a caller-
    /// provided [`IfaceMap`].
    pub fn bind_ephemeral_same_port_with_map(map: &IfaceMap, broadcast: bool) -> io::Result<Self> {
        let ifaces = map.all();
        let mut up_first: Vec<IfaceInfo> = Vec::with_capacity(ifaces.len());
        // Order matters: pick the port from a non-loopback NIC if one
        // exists, so the kernel assigns from a more meaningful pool
        // (and the loopback bind that reuses it is harmless either
        // way). Loopback and any remaining NICs follow.
        for i in &ifaces {
            if i.up_non_loopback {
                up_first.push(i.clone());
            }
        }
        for i in &ifaces {
            if !i.up_non_loopback {
                up_first.push(i.clone());
            }
        }
        let mut iter = up_first.into_iter();
        let first_info = iter
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no IPv4 NICs"))?;
        let first = bind_one(&first_info, 0, broadcast)?;
        let chosen_port = first
            .sock
            .local_addr()
            .ok()
            .map(|sa| sa.port())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "could not read chosen UDP port")
            })?;
        let mut sockets = vec![first];
        for info in iter {
            match bind_one(&info, chosen_port, broadcast) {
                Ok(nic) => sockets.push(nic),
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface = %info.ip,
                        port = chosen_port,
                        error = %e,
                        "skipping NIC: same-port bind failed"
                    );
                }
            }
        }
        Ok(Self { sockets })
    }

    /// Bind to a single specific interface IP. Useful when the caller
    /// has already decided which NIC should originate traffic (e.g.
    /// per-NIC SEARCH server responder tasks).
    pub fn bind_single(iface_ip: Ipv4Addr, port: u16, broadcast: bool) -> io::Result<Self> {
        let map = IfaceMap::new();
        let info = map
            .all()
            .into_iter()
            .find(|i| i.ip == iface_ip)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("AsyncUdpV4: iface {iface_ip} not found"),
                )
            })?;
        let nic = bind_one(&info, port, broadcast)?;
        Ok(Self { sockets: vec![nic] })
    }

    /// Inspect the per-NIC sockets — diagnostics + response correlation.
    pub fn ifaces(&self) -> &[NicSocket] {
        &self.sockets
    }

    /// Local addresses, one per NIC socket. Different ephemeral ports
    /// per socket when `bind(0, ..)` was used.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.sockets
            .iter()
            .filter_map(|n| n.sock.local_addr().ok())
            .collect()
    }

    /// Send to a unicast or per-subnet-broadcast destination via the
    /// best-matching NIC. The selection rule:
    ///
    /// 1. If `dest` falls within a NIC's subnet → use that NIC.
    /// 2. If `dest` equals a NIC's subnet broadcast → use that NIC.
    /// 3. If `dest` is loopback (`127/8`) → use the loopback NIC.
    /// 4. Otherwise pick the first up, non-loopback NIC.
    ///
    /// For `255.255.255.255` and IPv4 multicast destinations, prefer
    /// [`Self::fanout_to`] — `send_to` will pick a single NIC, which
    /// is rarely what you want.
    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
        let v4 = match dest {
            SocketAddr::V4(v) => v,
            SocketAddr::V6(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AsyncUdpV4 is IPv4-only",
                ));
            }
        };
        let nic = self.pick_nic(*v4.ip())?;
        nic.sock.send_to(buf, dest).await
    }

    /// Send via a specific NIC (matched by interface IP). Returns
    /// [`io::ErrorKind::AddrNotAvailable`] when no socket is bound to
    /// `iface_ip`.
    pub async fn send_via(
        &self,
        buf: &[u8],
        dest: SocketAddr,
        iface_ip: Ipv4Addr,
    ) -> io::Result<usize> {
        let nic = self
            .sockets
            .iter()
            .find(|n| n.iface_ip == iface_ip && !n.rx_only_bcast)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("AsyncUdpV4: no socket bound to {iface_ip}"),
                )
            })?;
        nic.sock.send_to(buf, dest).await
    }

    /// Send via the NIC whose `ifindex` matches. Fallback for
    /// callers that already track ifindex (e.g. server SEARCH
    /// responder using the cmsg-derived index from a future
    /// IP_PKTINFO upgrade). On Windows ifindex may be 0 for every
    /// NIC; in that case pass `None` and use [`Self::send_via`] with
    /// the iface IP instead.
    pub async fn send_via_ifindex(
        &self,
        buf: &[u8],
        dest: SocketAddr,
        ifindex: u32,
    ) -> io::Result<usize> {
        let nic = self
            .sockets
            .iter()
            .find(|n| n.ifindex == ifindex && n.ifindex != 0 && !n.rx_only_bcast)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("AsyncUdpV4: no socket with ifindex {ifindex}"),
                )
            })?;
        nic.sock.send_to(buf, dest).await
    }

    /// Send the same payload via every up, non-loopback NIC. Use for
    /// `255.255.255.255` and multicast destinations on multi-NIC
    /// hosts. Returns the number of sockets the send succeeded on
    /// (best-effort — per-NIC send errors are logged at `debug` and
    /// counted as failures).
    pub async fn fanout_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
        let mut ok_count = 0usize;
        let mut last_err: Option<io::Error> = None;
        for nic in &self.sockets {
            if nic.is_loopback || nic.rx_only_bcast {
                continue;
            }
            match nic.sock.send_to(buf, dest).await {
                Ok(_) => ok_count += 1,
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        %dest,
                        error = %e,
                        "fanout send failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok_count == 0 {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "AsyncUdpV4: fanout had no eligible NICs",
                )
            }));
        }
        Ok(ok_count)
    }

    /// Receive on whichever NIC's socket becomes ready first. Returns
    /// [`RecvMeta`] with the receiving NIC info synthesised.
    pub async fn recv_with_meta(&self, buf: &mut [u8]) -> io::Result<RecvMeta> {
        // Build one future per NIC socket. Each future owns its own
        // recv buffer; whichever fires first is copied into the
        // caller's buffer. We don't reuse a single buffer across all
        // sockets because `tokio::net::UdpSocket::recv_from` takes
        // `&mut [u8]`, and we'd need shared mutable access to merge.
        let mut futures = Vec::with_capacity(self.sockets.len());
        for nic in &self.sockets {
            let sock = nic.sock.clone();
            let info = (nic.iface_ip, nic.ifindex);
            futures.push(Box::pin(async move {
                let mut local = vec![0u8; 65535];
                let r = sock.recv_from(&mut local).await;
                (r, info, local)
            }));
        }
        if futures.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "AsyncUdpV4: no NIC sockets",
            ));
        }
        let ((res, info, local), _idx, _rest) = select_all_owned(futures).await;
        let (n, src) = res?;
        let copy_len = n.min(buf.len());
        buf[..copy_len].copy_from_slice(&local[..copy_len]);
        let (iface_ip, ifindex) = info;
        Ok(RecvMeta {
            n: copy_len,
            src,
            dst_ip: Some(iface_ip),
            ifindex: if ifindex == 0 { None } else { Some(ifindex) },
            iface_ip,
        })
    }

    /// Convenience equivalent to `tokio::net::UdpSocket::recv_from`.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let m = self.recv_with_meta(buf).await?;
        Ok((m.n, m.src))
    }

    /// Pick the NIC for a given destination IP using subnet/loopback
    /// rules. Public for callers (e.g. SEARCH engine) that want to
    /// preview the routing decision before sending.
    pub fn pick_nic(&self, dest: Ipv4Addr) -> io::Result<&NicSocket> {
        // RX-only broadcast sockets must never be used for sending.
        let send_eligible = || self.sockets.iter().filter(|n| !n.rx_only_bcast);
        // (1) Subnet match.
        for nic in send_eligible() {
            if subnet_contains(nic.iface_ip, nic.netmask, dest) {
                return Ok(nic);
            }
        }
        // (2) Per-subnet broadcast match.
        for nic in send_eligible() {
            if Some(dest) == nic.broadcast {
                return Ok(nic);
            }
        }
        // (3) Loopback.
        if dest.is_loopback() {
            if let Some(nic) = send_eligible().find(|n| n.is_loopback) {
                return Ok(nic);
            }
        }
        // (4) First non-loopback NIC.
        if let Some(nic) = send_eligible().find(|n| !n.is_loopback) {
            return Ok(nic);
        }
        // Last resort: first send-eligible NIC.
        send_eligible().next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "AsyncUdpV4: no NIC sockets",
            )
        })
    }

    /// Apply `SO_RCVBUF` to every per-NIC socket. CA / PVA SEARCH
    /// bursts can deliver hundreds of responses inside a few ms, so
    /// callers typically bump this above the kernel default.
    /// Per-NIC errors are logged at `debug`; the call returns Ok as
    /// long as the request didn't fail on every NIC.
    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        let mut ok = 0usize;
        let mut last_err: Option<io::Error> = None;
        for nic in &self.sockets {
            let sref = socket_ref(&nic.sock);
            match sref.set_recv_buffer_size(size) {
                Ok(()) => ok += 1,
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        size,
                        error = %e,
                        "set_recv_buffer_size failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok == 0 {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "AsyncUdpV4: set_recv_buffer_size had no eligible NICs",
                )
            }));
        }
        Ok(())
    }

    /// Apply an `IP_MULTICAST_TTL` value to every underlying NIC socket.
    ///
    /// Mirrors epics-base 3.16 commit f2a1834d (`EPICS_CA_MCAST_TTL`):
    /// when the destination of a UDP send is a multicast group the OS
    /// uses this TTL for the outgoing packet. Has no effect on unicast
    /// or limited-broadcast traffic, so it's safe to apply unconditionally
    /// on every CA / PVA UDP socket — callers don't have to know whether
    /// any particular send will hit a multicast destination.
    ///
    /// Errors on individual NICs are logged at `debug` and ignored; the
    /// call returns `Err` only when every NIC fails (so the caller can
    /// detect "TTL was applied nowhere"). `ttl` must already be in
    /// `1..=255` — pass `runtime::net::ca_mcast_ttl()` to honor the
    /// `EPICS_CA_MCAST_TTL` env var.
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
        let mut ok = 0usize;
        let mut last_err: Option<io::Error> = None;
        for nic in &self.sockets {
            match nic.sock.set_multicast_ttl_v4(ttl) {
                Ok(()) => ok += 1,
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        ttl,
                        error = %e,
                        "set_multicast_ttl_v4 failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok == 0 {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "AsyncUdpV4: set_multicast_ttl_v4 had no NICs",
                )
            }));
        }
        Ok(())
    }

    /// Opt every underlying NIC socket into the Linux `SO_RXQ_OVFL`
    /// receive-queue overflow counter (commit pvxs `a064677e3625`).
    /// When enabled the kernel attaches a `SOL_SOCKET / SO_RXQ_OVFL`
    /// `cmsg` to every received packet carrying a 32-bit running
    /// counter of how many datagrams were dropped because the
    /// per-socket receive buffer was full.
    ///
    /// On non-Linux platforms this is a no-op success — the kernel
    /// has no equivalent counter, so callers don't need to gate on
    /// `cfg(target_os = "linux")` themselves.
    ///
    /// Pair with [`AsyncUdpV4::recv_from_with_drop_count`] to surface
    /// the counter value.
    pub fn enable_so_rxq_ovfl(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            for nic in &self.sockets {
                let fd = nic.sock.as_raw_fd();
                let val: libc::c_int = 1;
                // SAFETY: fd is owned by the tokio UdpSocket for the
                // lifetime of `nic`; setsockopt on a non-blocking UDP
                // socket is sound.
                let r = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RXQ_OVFL,
                        &val as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&val) as libc::socklen_t,
                    )
                };
                if r != 0 {
                    let err = io::Error::last_os_error();
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        error = %err,
                        "enable_so_rxq_ovfl failed"
                    );
                }
            }
        }
        let _ = self;
        Ok(())
    }

    /// Receive one datagram and return the kernel's current
    /// `SO_RXQ_OVFL` drop counter alongside the usual `(size, src)`
    /// tuple. Caller is responsible for tracking deltas — the counter
    /// is a monotonically-increasing running total since the socket
    /// was opened (or since the option was enabled).
    ///
    /// On non-Linux platforms or when [`Self::enable_so_rxq_ovfl`]
    /// was never called, returns `drop_count = 0`. Callers should
    /// log only on transitions (`prev != current && current != 0`)
    /// to avoid spamming on every ordinary packet.
    ///
    /// This receives on the FIRST NIC's socket only — designed for
    /// the single-binding cases (e.g. PVA UDP collector). The
    /// multi-NIC `recv_with_meta` path uses `tokio::select!` across
    /// every NIC and would need its own per-NIC drop tracking; that
    /// integration is intentionally deferred.
    pub async fn recv_from_with_drop_count(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, u32)> {
        let nic = self
            .sockets
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no NIC sockets"))?;
        recv_from_with_drop_count_one(&nic.sock, buf).await
    }
}

/// Per-socket recv that surfaces the `SO_RXQ_OVFL` cmsg value on
/// Linux. On other platforms this is a thin wrapper over
/// `tokio::net::UdpSocket::recv_from` returning `drop_count = 0`.
#[cfg(target_os = "linux")]
async fn recv_from_with_drop_count_one(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, u32)> {
    use std::os::fd::AsRawFd;

    loop {
        sock.readable().await?;
        let raw_fd = sock.as_raw_fd();

        // try_io: drives one non-blocking recvmsg and yields the
        // result. `Err(WouldBlock)` from the closure tells tokio to
        // re-register and resume waiting on the next readable.
        let res = sock.try_io(tokio::io::Interest::READABLE, || {
            // SAFETY: the iovec / msghdr / cmsghdr scaffolding is
            // local to this closure and does not outlive recvmsg.
            // The destination buffers are borrowed for the duration
            // of the call.
            unsafe {
                let mut storage: libc::sockaddr_storage = std::mem::zeroed();
                let mut iov = libc::iovec {
                    iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                    iov_len: buf.len(),
                };
                // CMSG_SPACE(4) padded for alignment — one 32-bit
                // SO_RXQ_OVFL value is the only ancillary we ask for.
                let mut cbuf = [0u8; 64];
                let mut msg: libc::msghdr = std::mem::zeroed();
                msg.msg_name = &mut storage as *mut _ as *mut libc::c_void;
                msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
                msg.msg_controllen = cbuf.len() as _;

                let n = libc::recvmsg(raw_fd, &mut msg, 0);
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }

                // Decode the source address.
                let src = sockaddr_storage_to_socketaddr(&storage, msg.msg_namelen)?;

                // Walk the cmsg list for SOL_SOCKET / SO_RXQ_OVFL.
                let mut drops: u32 = 0;
                let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&msg);
                while !cmsg_ptr.is_null() {
                    let cmsg = &*cmsg_ptr;
                    if cmsg.cmsg_level == libc::SOL_SOCKET
                        && cmsg.cmsg_type == libc::SO_RXQ_OVFL
                        && cmsg.cmsg_len as usize
                            >= libc::CMSG_LEN(std::mem::size_of::<u32>() as u32) as usize
                    {
                        let data_ptr = libc::CMSG_DATA(cmsg_ptr) as *const u32;
                        drops = std::ptr::read_unaligned(data_ptr);
                    }
                    cmsg_ptr = libc::CMSG_NXTHDR(&msg, cmsg_ptr);
                }

                Ok((n as usize, src, drops))
            }
        });

        match res {
            Ok(out) => return out,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn recv_from_with_drop_count_one(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, u32)> {
    let (n, src) = sock.recv_from(buf).await?;
    Ok((n, src, 0))
}

#[cfg(target_os = "linux")]
unsafe fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> io::Result<SocketAddr> {
    use std::net::Ipv6Addr;
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "AF_INET sockaddr too short",
                ));
            }
            let sa = &*(storage as *const _ as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
            let port = u16::from_be(sa.sin_port);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in6>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "AF_INET6 sockaddr too short",
                ));
            }
            let sa = &*(storage as *const _ as *const libc::sockaddr_in6);
            let ip = Ipv6Addr::from(sa.sin6_addr.s6_addr);
            let port = u16::from_be(sa.sin6_port);
            Ok(SocketAddr::new(ip.into(), port))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported sockaddr family",
        )),
    }
}

impl AsyncUdpV4 {
    /// Join a multicast group on every up, non-loopback NIC. Errors
    /// per-NIC are logged at `debug` and not propagated unless every
    /// join fails.
    pub fn join_multicast_v4(&self, group: Ipv4Addr) -> io::Result<()> {
        let mut ok = 0usize;
        let mut last_err: Option<io::Error> = None;
        for nic in &self.sockets {
            if nic.is_loopback || nic.rx_only_bcast {
                continue;
            }
            match nic.sock.join_multicast_v4(group, nic.iface_ip) {
                Ok(()) => ok += 1,
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        %group,
                        error = %e,
                        "join_multicast_v4 failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok == 0 {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "AsyncUdpV4: join_multicast_v4 had no eligible NICs",
                )
            }));
        }
        Ok(())
    }
}

/// Build a [`socket2::SockRef`] borrowing `sock`'s file descriptor /
/// SOCKET handle. Used to apply socket options after the
/// `tokio::net::UdpSocket` is already constructed.
fn socket_ref(sock: &UdpSocket) -> socket2::SockRef<'_> {
    // socket2 0.5+ implements `From<&T>` for any `T: AsFd` (Unix) or
    // `T: AsSocket` (Windows). `tokio::net::UdpSocket` satisfies both.
    socket2::SockRef::from(sock)
}

fn bind_one(info: &IfaceInfo, port: u16, broadcast: bool) -> io::Result<NicSocket> {
    bind_one_at(info, info.ip, port, broadcast, false)
}

/// Bind to an arbitrary IPv4 address while keeping the NIC metadata
/// from `info`. Used by [`bind_with_map`] to create both the
/// primary unicast socket (`bind_ip = info.ip`) and an auxiliary
/// broadcast-RX socket (`bind_ip = info.broadcast`).
fn bind_one_at(
    info: &IfaceInfo,
    bind_ip: Ipv4Addr,
    port: u16,
    broadcast: bool,
    rx_only_bcast: bool,
) -> io::Result<NicSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Mirror EPICS-base `epicsSocketEnableAddressUseForDatagramFanout`
    // (libcom/src/osi/os/default/osdSockAddrReuse.cpp): on Unix, both
    // SO_REUSEADDR and SO_REUSEPORT are needed so a PVA server and
    // client (or two PVA processes) on the same host can co-bind the
    // same per-NIC (IP, 5076). Without this, the second process gets
    // EADDRINUSE on every NIC except loopback, search packets never
    // reach the server, and reconnect after IOC restart silently
    // fails.
    //
    // libcom commit 19146a5: Windows SO_REUSEADDR has dangerous
    // socket-hijack semantics (any process can rebind), and Windows
    // releases ports immediately on close anyway, so the flag is
    // skipped on Windows. The Windows-idiomatic alternative is
    // SO_EXCLUSIVEADDRUSE, but plain bind() already prevents reuse.
    #[cfg(not(windows))]
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    if broadcast {
        sock.set_broadcast(true)?;
    }
    // Linux: a per-NIC bound socket should not pick up multicast
    // delivered on a different NIC. libcom 51191e6155.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }
    sock.set_nonblocking(true)?;
    let bind_addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new(bind_ip, port));
    sock.bind(&bind_addr.into())?;
    let std_sock: std::net::UdpSocket = sock.into();
    let tokio_sock = UdpSocket::from_std(std_sock)?;
    Ok(NicSocket {
        sock: Arc::new(tokio_sock),
        iface_ip: info.ip,
        ifindex: info.index,
        netmask: info.netmask,
        broadcast: info.broadcast,
        is_loopback: info.ip.is_loopback(),
        rx_only_bcast,
    })
}

fn subnet_contains(ip: Ipv4Addr, mask: Ipv4Addr, candidate: Ipv4Addr) -> bool {
    let m = u32::from(mask);
    if m == 0 {
        return false;
    }
    (u32::from(ip) & m) == (u32::from(candidate) & m)
}

/// Hand-rolled `select_all` for owned, pinned futures. Avoids pulling
/// `futures-util` into `epics-base-rs` for a single use site.
async fn select_all_owned<F, T>(
    mut futures: Vec<std::pin::Pin<Box<F>>>,
) -> (T, usize, Vec<std::pin::Pin<Box<F>>>)
where
    F: std::future::Future<Output = T> + ?Sized,
{
    use std::future::poll_fn;
    use std::task::Poll;
    let (out, idx) = poll_fn(|cx| {
        for (i, fut) in futures.iter_mut().enumerate() {
            if let Poll::Ready(v) = fut.as_mut().poll(cx) {
                return Poll::Ready((v, i));
            }
        }
        Poll::Pending
    })
    .await;
    let _completed = futures.swap_remove(idx);
    (out, idx, futures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_send_and_recv() {
        let sender = AsyncUdpV4::bind(0, false).expect("sender bind");
        let receiver = AsyncUdpV4::bind(0, false).expect("receiver bind");

        // Find the receiver's loopback bound port.
        let lo_addr = receiver
            .ifaces()
            .iter()
            .find(|n| n.is_loopback)
            .map(|n| n.sock.local_addr().unwrap())
            .expect("loopback NIC must exist");

        let payload = b"libca-fanout";
        let _n = sender.send_to(payload, lo_addr).await.expect("send to lo");

        let mut buf = [0u8; 64];
        let meta = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_with_meta(&mut buf),
        )
        .await
        .expect("recv timeout")
        .expect("recv ok");
        assert_eq!(meta.n, payload.len());
        assert_eq!(&buf[..meta.n], payload);
        assert!(
            meta.iface_ip.is_loopback(),
            "expected loopback iface_ip, got {:?}",
            meta.iface_ip
        );
    }

    #[tokio::test]
    async fn send_via_loopback_iface_ip() {
        let sock = AsyncUdpV4::bind(0, false).expect("bind");
        let lo_iface = sock
            .ifaces()
            .iter()
            .find(|n| n.is_loopback)
            .expect("loopback NIC must exist")
            .iface_ip;

        let receiver = AsyncUdpV4::bind(0, false).expect("recv bind");
        let dest = receiver
            .ifaces()
            .iter()
            .find(|n| n.is_loopback)
            .map(|n| n.sock.local_addr().unwrap())
            .unwrap();

        let n = sock.send_via(b"x", dest, lo_iface).await.expect("send_via");
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn bind_ephemeral_same_port_uses_one_port_across_nics() {
        let sock = AsyncUdpV4::bind_ephemeral_same_port(false).expect("bind same-port");
        let ports: Vec<u16> = sock
            .ifaces()
            .iter()
            .filter_map(|n| n.sock.local_addr().ok().map(|sa| sa.port()))
            .collect();
        assert!(!ports.is_empty(), "at least one bound port");
        // Every per-NIC socket shares the same port.
        let first = ports[0];
        for p in &ports {
            assert_eq!(*p, first, "all NIC sockets must share one port");
        }
        assert!(first != 0, "ephemeral port must be non-zero");
    }

    #[tokio::test]
    async fn send_via_unknown_iface_returns_addr_not_available() {
        let sock = AsyncUdpV4::bind(0, false).expect("bind");
        let bogus = Ipv4Addr::new(203, 0, 113, 99);
        let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999));
        let err = sock
            .send_via(b"x", dest, bogus)
            .await
            .expect_err("unknown iface must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[tokio::test]
    async fn pick_nic_loopback() {
        // `bind` ends up calling `tokio::net::UdpSocket::from_std`, which
        // requires a Tokio runtime — hence #[tokio::test].
        let sock = AsyncUdpV4::bind(0, false).expect("bind");
        let nic = sock.pick_nic(Ipv4Addr::LOCALHOST).expect("pick");
        assert!(nic.is_loopback || nic.iface_ip.is_loopback());
    }

    #[test]
    fn subnet_contains_basic() {
        let ip = Ipv4Addr::new(10, 0, 0, 5);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        assert!(subnet_contains(ip, mask, Ipv4Addr::new(10, 0, 0, 99)));
        assert!(!subnet_contains(ip, mask, Ipv4Addr::new(10, 0, 1, 1)));
        // Zero mask must NOT match (would otherwise let any dest map
        // to any iface, defeating routing decisions).
        assert!(!subnet_contains(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(8, 8, 8, 8)
        ));
    }

    /// pvxs `a064677e3625` parity: `enable_so_rxq_ovfl` must be a
    /// no-op success on every platform — Linux opts the kernel into
    /// the SO_RXQ_OVFL counter, non-Linux returns Ok with no
    /// behaviour change.
    #[tokio::test]
    async fn enable_so_rxq_ovfl_is_no_op_success_off_linux() {
        let sock = AsyncUdpV4::bind(0, false).expect("bind");
        sock.enable_so_rxq_ovfl()
            .expect("enable_so_rxq_ovfl must succeed on every platform");
    }

    /// `recv_from_with_drop_count` returns `drop_count = 0` on a
    /// freshly-bound socket whose receive buffer never overflowed.
    /// The src/n payload must round-trip through the recvmsg path
    /// identically to `recv_from`.
    #[tokio::test]
    async fn recv_from_with_drop_count_returns_zero_under_normal_load() {
        let server = AsyncUdpV4::bind(0, false).expect("server bind");
        server.enable_so_rxq_ovfl().expect("enable counter");
        let server_port = server.sockets[0]
            .sock
            .local_addr()
            .expect("local_addr")
            .port();

        let client = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("client bind");
        let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port));
        let payload = b"hello-rxq-ovfl";
        client.send_to(payload, dest).await.expect("send");

        let mut buf = [0u8; 64];
        let (n, src, drops) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.recv_from_with_drop_count(&mut buf),
        )
        .await
        .expect("recv timeout")
        .expect("recv ok");
        assert_eq!(n, payload.len(), "byte count must match");
        assert_eq!(&buf[..n], payload, "payload must round-trip");
        assert!(
            src.port() != 0,
            "src port must be the client's ephemeral port, got {src:?}"
        );
        assert_eq!(
            drops, 0,
            "freshly-bound socket must report 0 drops; got {drops}"
        );
    }
}
