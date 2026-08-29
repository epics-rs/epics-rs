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

// RTEMS-EXEC-MODEL-ALLOW(17): every test binds tokio::net UDP sockets, which
// need a reactor the exec backend does not start; on the exec backend these
// still run on the tokio driver `#[tokio::test]` builds, and pass. b3ab3ab8
// added the three bundle-port tests without touching the count; all 17 were
// re-run under `EPICS_RS_BUILD_EXEC_BACKEND=thread` before this bump. Each
// keeps its own attribute rather than sharing a helper, so the count tracks
// the test count and the next addition fails closed.
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
    /// `rsrv/caservertask.c:671-709`). Send paths skip these sockets.
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
    /// How this bundle's port was obtained, recorded at bind time.
    ///
    /// A socket joined to the bundle later ([`Self::ensure_iface_socket`])
    /// inherits it rather than re-deriving it from the number: once the
    /// kernel has answered a `bind(0)`, the number it handed back is
    /// indistinguishable from one an operator configured, and only the
    /// bundle still knows which it was.
    port_use: PortUse,
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
        Self::bind_with_map(&IfaceMap::new()?, port, broadcast)
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
        Self::bind_with_map_filtered(&IfaceMap::new()?, port, broadcast, true, None)
    }

    /// Like [`Self::bind`] but binds **only** the interfaces whose
    /// unicast IPv4 address appears in `interfaces`, in list order —
    /// the all-NIC default is *replaced* by exactly the configured set.
    ///
    /// This is how `EPICS_PVA*_INTF_ADDR_LIST` constrains a responder /
    /// search socket to specific interfaces (pvxs `server.cpp:407-439`,
    /// `config.cpp:624-648`): the server binds only the addresses the
    /// operator listed, not every discovered NIC. A listed address that
    /// is not a currently-enumerated local NIC is bound directly as a
    /// unicast-only interface (no broadcast-RX socket), so a
    /// loopback-only list (`127.0.0.1`) binds exactly one loopback
    /// socket and produces no non-loopback exposure. Pass an empty slice
    /// only via the all-NIC entry points ([`Self::bind`]); here an empty
    /// `interfaces` would bind nothing and error.
    pub fn bind_on_interfaces(
        interfaces: &[Ipv4Addr],
        port: u16,
        broadcast: bool,
    ) -> io::Result<Self> {
        Self::bind_with_map_filtered(&IfaceMap::new()?, port, broadcast, false, Some(interfaces))
    }

    /// Like [`Self::bind`] but reuses an existing [`IfaceMap`] —
    /// useful when callers maintain a long-lived shared map.
    pub fn bind_with_map(map: &IfaceMap, port: u16, broadcast: bool) -> io::Result<Self> {
        Self::bind_with_map_filtered(map, port, broadcast, false, None)
    }

    fn bind_with_map_filtered(
        map: &IfaceMap,
        port: u16,
        broadcast: bool,
        skip_loopback: bool,
        only: Option<&[Ipv4Addr]>,
    ) -> io::Result<Self> {
        let ifaces = select_ifaces(map, only);
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
            // (`caservertask.c:671-709`) and bind a second RX-only
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
                    match bind_one_at(
                        &info,
                        bcast,
                        port,
                        PortUse::from_caller(port),
                        broadcast,
                        true,
                    ) {
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
        Ok(Self {
            sockets,
            port_use: PortUse::from_caller(port),
        })
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
        Self::bind_ephemeral_same_port_with_map(&IfaceMap::new()?, broadcast)
    }

    /// Like [`Self::bind_ephemeral_same_port`] but reuses a caller-
    /// provided [`IfaceMap`].
    pub fn bind_ephemeral_same_port_with_map(map: &IfaceMap, broadcast: bool) -> io::Result<Self> {
        Self::bind_ephemeral_same_port_inner(map, broadcast, None)
    }

    /// Like [`Self::bind_ephemeral_same_port`] but binds **only** the
    /// interfaces whose unicast IPv4 address appears in `interfaces` —
    /// the same-port-per-NIC bundle is restricted to exactly the
    /// configured set rather than every discovered NIC, so a
    /// loopback-only list binds a single loopback socket.
    ///
    /// NOTE: the PVA client active-search socket does NOT bind through
    /// this to apply `EPICS_PVA_INTF_ADDR_LIST`. pvxs binds the search
    /// socket to wildcard (`client.cpp:578-590`) and applies the
    /// interface list only to auto-broadcast expansion and fanout egress;
    /// reducing the bundle here forced an explicit non-loopback
    /// `EPICS_PVA_ADDR_LIST` target onto a loopback-only socket. This is a
    /// general primitive for callers that genuinely need a bundle bound to
    /// a fixed interface set.
    pub fn bind_ephemeral_same_port_on_interfaces(
        interfaces: &[Ipv4Addr],
        broadcast: bool,
    ) -> io::Result<Self> {
        Self::bind_ephemeral_same_port_inner(&IfaceMap::new()?, broadcast, Some(interfaces))
    }

    /// How many distinct ephemeral ports the bundle will try before it accepts
    /// a degraded one. Each attempt is one `bind(0)` plus one bind per
    /// remaining NIC, so the cost of the whole ladder is microseconds; the cost
    /// of accepting the first partial bundle was a channel that never connects.
    const BUNDLE_ATTEMPTS: usize = 8;

    fn bind_ephemeral_same_port_inner(
        map: &IfaceMap,
        broadcast: bool,
        only: Option<&[Ipv4Addr]>,
    ) -> io::Result<Self> {
        let ifaces = select_ifaces(map, only);
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
        let total_nics = up_first.len();
        let expected_non_loopback = up_first.iter().filter(|i| i.up_non_loopback).count();
        if up_first.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no IPv4 NICs",
            ));
        }

        // One port on every NIC is the whole contract of this bundle, and a
        // partial bundle used to be accepted on the first collision: the
        // kernel picks the port from NIC 1 alone, so nothing about it says the
        // number is free on NIC 2, and under a workspace test run — dozens of
        // processes each taking an ephemeral port — a collision is ordinary.
        // The caller then searches on a NIC short of the one the server
        // answers on and the channel never connects, expiring against whatever
        // timeout the surrounding operation happens to own. Retry the *whole*
        // bundle with a fresh port instead, keeping each failed attempt's
        // first socket parked so the kernel cannot hand back the number that
        // just collided.
        // Every attempt is kept until the end: an attempt still holding its
        // colliding port is what stops the next `bind(0)` handing the same
        // number straight back.
        let mut attempts: Vec<(Vec<NicSocket>, usize)> = Vec::new();
        let mut last_drop: Option<(Ipv4Addr, io::Error)> = None;

        for _ in 0..Self::BUNDLE_ATTEMPTS {
            let mut iter = up_first.iter();
            let first_info = iter.next().expect("checked non-empty");
            let first = bind_one(first_info, 0, broadcast)?;
            let chosen_port = first
                .sock
                .local_addr()
                .ok()
                .map(|sa| sa.port())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Other, "could not read chosen UDP port")
                })?;
            let mut sockets = vec![first];
            let mut dropped = 0usize;
            for info in iter {
                // Exclusive: this port is ephemeral however non-zero it looks,
                // so a number somebody already holds must be EADDRINUSE here
                // rather than a silent join of their fanout group.
                match bind_one_exclusive(info, chosen_port, broadcast) {
                    Ok(nic) => sockets.push(nic),
                    Err(e) => {
                        dropped += 1;
                        tracing::debug!(
                            target: "epics_base_rs::net",
                            iface = %info.ip,
                            port = chosen_port,
                            error = %e,
                            "skipping NIC: same-port bind failed"
                        );
                        last_drop = Some((info.ip, e));
                    }
                }
            }
            if dropped == 0 {
                return Ok(Self {
                    sockets,
                    port_use: PortUse::Exclusive,
                });
            }
            attempts.push((sockets, dropped));
        }

        // Every port collided somewhere. Keep the least-degraded attempt and
        // let the rest — and the ports they were holding — go.
        let best = attempts
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, d))| *d)
            .map(|(i, _)| i)
            .expect("at least one attempt ran");
        let (sockets, dropped) = attempts.swap_remove(best);
        drop(attempts);

        // Every port tried collided on the same NIC(s): this is a standing
        // condition on the host, not a race, so report it once and hand back
        // the least-degraded bundle rather than looping forever.
        let bound_non_loopback = sockets.iter().filter(|n| !n.is_loopback).count();
        tracing::warn!(
            target: "epics_base_rs::net",
            port = sockets.first().and_then(|n| n.sock.local_addr().ok()).map(|sa| sa.port()),
            total_nics,
            bound = sockets.len(),
            dropped,
            bound_non_loopback,
            attempts = Self::BUNDLE_ATTEMPTS,
            last_iface = ?last_drop.as_ref().map(|(ip, _)| *ip),
            last_error = ?last_drop.as_ref().map(|(_, e)| e.to_string()),
            "bind_ephemeral_same_port: some NICs failed the same-port bind on every port tried; \
             SEARCH/beacon fanout is degraded"
        );

        // Hard failure: non-loopback NICs were available but none of
        // them bound — the bundle cannot fan out at all. Mirror
        // `bind_with_map_filtered`, which errors when no usable socket
        // is left, instead of returning a silently single-loopback
        // bundle.
        if expected_non_loopback > 0 && bound_non_loopback == 0 {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "bind_ephemeral_same_port: {expected_non_loopback} non-loopback NIC(s) \
                     present but none could bind an ephemeral UDP port in {} attempts",
                    Self::BUNDLE_ATTEMPTS
                ),
            ));
        }
        Ok(Self {
            sockets,
            port_use: PortUse::Exclusive,
        })
    }

    /// Bind to a single specific interface IP. Useful when the caller
    /// has already decided which NIC should originate traffic (e.g.
    /// per-NIC SEARCH server responder tasks).
    pub fn bind_single(iface_ip: Ipv4Addr, port: u16, broadcast: bool) -> io::Result<Self> {
        let map = IfaceMap::new()?;
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
        Ok(Self {
            sockets: vec![nic],
            port_use: PortUse::from_caller(port),
        })
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

    /// Send a multicast datagram applying the pvxs per-endpoint options
    /// (`evhelper.cpp:556-577`): the multicast TTL and the outgoing interface.
    ///
    /// PIN-ONLY: `:556-577` opens `mcast_prep_sendto` (`:556-592`) at the pvxs
    /// pin `b568e93` (1.5.1-42) and runs to its IPv4 `IP_MULTICAST_IF` log. At
    /// this machine's pvxs HEAD `bd2243d` (1.5.2-26) the same span is
    /// `:561-582`, because five lines land ahead of it — `:556` there is inside
    /// `mcast_leave`. A repin to HEAD must add 5; leaving the number is what
    /// makes it wrong.
    ///
    /// - **Outgoing interface** is selected structurally by sending from the
    ///   NIC socket bound to `iface_ip` — each NIC socket is bound to its NIC's
    ///   address, so source-address routing forces multicast egress out that
    ///   interface, making an `IP_MULTICAST_IF` setsockopt redundant (it is not
    ///   set). `iface_ip == None` fans the datagram out every eligible NIC,
    ///   matching the modifier-less multicast/broadcast behaviour of
    ///   [`Self::fanout_to`].
    /// - **TTL** (`IP_MULTICAST_TTL`) is applied to each target NIC socket
    ///   immediately before its send and re-applied on every call, so a
    ///   previous endpoint's TTL never sticks to a later one (pvxs sets it
    ///   per-send too).
    ///
    /// CONTRACT: `IP_MULTICAST_TTL` is per-socket sticky state, not a
    /// per-datagram option, so the caller MUST be the sole multicast sender on
    /// the selected NIC socket(s). The PVA beacon task satisfies this: it is
    /// the only task that issues multicast sends on the responder sockets and
    /// its send loop is sequential; every other send on those sockets is
    /// unicast, which ignores `IP_MULTICAST_TTL`. Do not call this from
    /// multiple tasks sharing a NIC socket without external serialisation.
    ///
    /// Returns the number of NIC sockets the datagram was sent on; errors only
    /// when no eligible NIC sent it (e.g. `iface_ip` matched no bound NIC).
    pub async fn send_multicast_v4(
        &self,
        buf: &[u8],
        dest: SocketAddr,
        ttl: u32,
        iface_ip: Option<Ipv4Addr>,
    ) -> io::Result<usize> {
        let mut ok_count = 0usize;
        let mut last_err: Option<io::Error> = None;
        for nic in &self.sockets {
            if nic.is_loopback || nic.rx_only_bcast {
                continue;
            }
            if let Some(want) = iface_ip {
                if nic.iface_ip != want {
                    continue;
                }
            }
            // Apply IP_MULTICAST_TTL on this NIC immediately before the send
            // (re-applied every call; see the method contract). A failure here
            // is non-fatal — the datagram still egresses, at the prior TTL.
            if let Err(e) = nic.sock.set_multicast_ttl_v4(ttl) {
                tracing::debug!(
                    target: "epics_base_rs::net",
                    iface_ip = %nic.iface_ip,
                    ttl,
                    error = %e,
                    "per-endpoint set_multicast_ttl_v4 failed; sending at prior TTL"
                );
            }
            match nic.sock.send_to(buf, dest).await {
                Ok(_) => ok_count += 1,
                Err(e) => {
                    tracing::debug!(
                        target: "epics_base_rs::net",
                        iface_ip = %nic.iface_ip,
                        %dest,
                        ttl,
                        error = %e,
                        "multicast send failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok_count == 0 {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    match iface_ip {
                        Some(ip) => {
                            format!("AsyncUdpV4: no eligible NIC bound to {ip} for multicast")
                        }
                        None => "AsyncUdpV4: multicast had no eligible NICs".to_string(),
                    },
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
        // (4) Default-route NIC — an interface with a `0.0.0.0`
        // netmask matches every destination. `subnet_contains` rejects
        // it in pass (1) so it never shadows a specific subnet; here it
        // is the explicit fallback for an otherwise-unrouted dest.
        if let Some(nic) = send_eligible().find(|n| !n.is_loopback && is_default_route(n.netmask)) {
            return Ok(nic);
        }
        // (5) First non-loopback NIC.
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
    /// the single-binding cases (e.g. PVA UDP collector). For the
    /// multi-NIC fan-in case, see [`Self::recv_with_meta_with_drops`].
    pub async fn recv_from_with_drop_count(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, u32)> {
        let nic = self
            .sockets
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no NIC sockets"))?;
        recv_from_with_drop_count_socket(&nic.sock, buf).await
    }

    /// Multi-NIC `recv_with_meta` variant that also returns the
    /// receiving NIC's `SO_RXQ_OVFL` drop counter at the moment this
    /// packet arrived. Caller tracks deltas keyed by NIC `iface_ip`
    /// (already exposed in [`RecvMeta::iface_ip`]) — a transition in
    /// any NIC's counter signals that the kernel just dropped at
    /// least one earlier datagram on that NIC because the per-socket
    /// receive buffer was full.
    ///
    /// On non-Linux platforms `drop_count` is always 0.
    pub async fn recv_with_meta_with_drops(&self, buf: &mut [u8]) -> io::Result<(RecvMeta, u32)> {
        // Build one future per NIC socket, each calling the cmsg-aware
        // single-socket recv. `select_all_owned` returns the first
        // future to complete; the others are dropped (and their local
        // buffers along with them) without leaking pending recvmsg
        // calls — same lifetime contract as the plain recv_with_meta.
        let mut futures = Vec::with_capacity(self.sockets.len());
        for nic in &self.sockets {
            let sock = nic.sock.clone();
            let info = (nic.iface_ip, nic.ifindex);
            futures.push(Box::pin(async move {
                let mut local = vec![0u8; 65535];
                let r = recv_from_with_drop_count_socket(&sock, &mut local).await;
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
        let (n, src, drops) = res?;
        let copy_len = n.min(buf.len());
        buf[..copy_len].copy_from_slice(&local[..copy_len]);
        let (iface_ip, ifindex) = info;
        Ok((
            RecvMeta {
                n: copy_len,
                src,
                dst_ip: Some(iface_ip),
                ifindex: if ifindex == 0 { None } else { Some(ifindex) },
                iface_ip,
            },
            drops,
        ))
    }
}

/// Free-function recv that surfaces the `SO_RXQ_OVFL` cmsg value on
/// Linux. Exposed so callers managing their own
/// `tokio::net::UdpSocket` (e.g. the PVA loopback ORIGIN_TAG socket)
/// can use the same overflow detection without going through
/// `AsyncUdpV4`. On non-Linux platforms this is a thin wrapper over
/// `tokio::net::UdpSocket::recv_from` returning `drop_count = 0`.
pub async fn recv_from_with_drop_count_socket(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, u32)> {
    recv_from_with_drop_count_one(sock, buf).await
}

/// Companion setter to [`recv_from_with_drop_count_socket`] for
/// callers that bind their own `tokio::net::UdpSocket`. Linux:
/// `setsockopt(SOL_SOCKET, SO_RXQ_OVFL, 1)`. Non-Linux: no-op
/// success.
pub fn enable_so_rxq_ovfl_for_socket(sock: &UdpSocket) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        let val: libc::c_int = 1;
        // SAFETY: fd is owned by the tokio UdpSocket borrowed for the
        // duration of this call; setsockopt on a non-blocking UDP
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
            return Err(io::Error::last_os_error());
        }
    }
    let _ = sock;
    Ok(())
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
            Ok(out) => return Ok(out),
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
            // SAFETY: caller guarantees `storage` is initialized and
            // `len` is at least size_of::<sockaddr_in>() (checked above);
            // the cast targets the C-layout struct matching AF_INET.
            let sa = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
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
            // SAFETY: caller guarantees `storage` is initialized and
            // `len` is at least size_of::<sockaddr_in6>() (checked above);
            // the cast targets the C-layout struct matching AF_INET6.
            let sa = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
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

    /// Join `group` on **only** the bundle socket bound to `iface_ip`,
    /// not on every NIC like [`Self::join_multicast_v4`].
    ///
    /// This is the per-interface multicast join pvxs performs when a
    /// multicast endpoint carries an explicit `@iface` modifier
    /// (`udp_collector.cpp:186-196`, `mcast_join` on the one chosen
    /// interface) — the group is received on that NIC alone. Returns
    /// [`io::ErrorKind::NotFound`] when no (non-`rx_only_bcast`) bundle
    /// socket is bound to `iface_ip`; the caller decides whether to first
    /// [`Self::ensure_iface_socket`] (server — pvxs adds listener
    /// interfaces outside the configured set) or to skip (client).
    pub fn join_multicast_v4_on(&self, group: Ipv4Addr, iface_ip: Ipv4Addr) -> io::Result<()> {
        let nic = self
            .sockets
            .iter()
            .find(|n| n.iface_ip == iface_ip && !n.rx_only_bcast)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("AsyncUdpV4: no socket bound to {iface_ip} to join {group}"),
                )
            })?;
        nic.sock.join_multicast_v4(group, iface_ip)
    }

    /// Ensure the bundle has a (non-`rx_only_bcast`) socket bound to
    /// `iface_ip` on `port`, binding and appending one if absent.
    ///
    /// This lets the server join a multicast group whose `@iface` lies
    /// *outside* `EPICS_PVAS_INTF_ADDR_LIST`: pvxs `addGroups`
    /// (`config.cpp:310-335`) pushes each beacon-destination join target
    /// onto the listener interface set regardless of the configured
    /// interface constraint, and `UDPCollector::addListener` opens a
    /// collector socket on that interface. The newly-bound socket is
    /// appended to `self.sockets`, so it participates in every subsequent
    /// `recv*` call (the receive loop iterates the whole bundle).
    ///
    /// Returns `Ok(true)` when a socket was added, `Ok(false)` when one
    /// was already present.
    pub fn ensure_iface_socket(
        &mut self,
        iface_ip: Ipv4Addr,
        port: u16,
        broadcast: bool,
    ) -> io::Result<bool> {
        if self
            .sockets
            .iter()
            .any(|n| n.iface_ip == iface_ip && !n.rx_only_bcast)
        {
            return Ok(false);
        }
        let map = IfaceMap::new()?;
        // `select_ifaces` always yields one entry for a single-element
        // list (synthesizing a unicast-only `IfaceInfo` when the OS did
        // not enumerate the address), so `next()` is always `Some`.
        let info = select_ifaces(&map, Some(&[iface_ip]))
            .into_iter()
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("AsyncUdpV4: cannot resolve interface {iface_ip}"),
                )
            })?;
        // `port` here is whatever the bundle resolved to, which for an
        // ephemeral bundle is the kernel's own answer — `bind_one`'s
        // `port != 0` reading would call that shareable and hand this NIC's
        // socket the fanout flags, letting a stranger's reuse group swallow
        // the port the rest of the bundle owns exclusively. Inherit the
        // bundle's recorded intent instead.
        let nic = bind_one_at(&info, info.ip, port, self.port_use, broadcast, false)?;
        self.sockets.push(nic);
        Ok(true)
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

/// Resolve which NICs a bind should cover.
///
/// `None` → every NIC `map.all()` enumerates (the all-interface
/// default; `EPICS_PVA*_INTF_ADDR_LIST` unset). `Some(list)` → only the
/// NICs whose unicast IPv4 address appears in `list`, returned in `list`
/// order. A listed address that is not a currently-enumerated local NIC
/// is synthesized as a unicast-only [`IfaceInfo`] (no subnet broadcast),
/// so binding still happens for an address the OS enumerator omitted
/// (e.g. an alias) and a loopback-only list yields exactly the loopback
/// interface. This is the single point where the interface-list
/// constraint is applied, shared by the fixed-port and ephemeral-same-
/// port bind paths.
fn select_ifaces(map: &IfaceMap, only: Option<&[Ipv4Addr]>) -> Vec<IfaceInfo> {
    let all = map.all();
    match only {
        None => all,
        Some(list) => list
            .iter()
            .map(|want| {
                all.iter()
                    .find(|i| i.ip == *want)
                    .cloned()
                    .unwrap_or_else(|| IfaceInfo {
                        index: 0,
                        name: String::new(),
                        ip: *want,
                        netmask: Ipv4Addr::UNSPECIFIED,
                        broadcast: None,
                        up_non_loopback: !want.is_loopback(),
                    })
            })
            .collect(),
    }
}

fn bind_one(info: &IfaceInfo, port: u16, broadcast: bool) -> io::Result<NicSocket> {
    bind_one_at(
        info,
        info.ip,
        port,
        PortUse::from_caller(port),
        broadcast,
        false,
    )
}

/// What the caller means the port to be, which is *not* the same question as
/// whether the number is zero.
///
/// The reuse flags used to be chosen by `port != 0`, and that reading is sound
/// only for a number the caller supplied: 0 means "give me one nobody else
/// has", anything else means "the well-known port every EPICS process
/// co-binds". It is wrong for a number this module chose itself — the port the
/// kernel handed back for the first NIC of an ephemeral bundle is still
/// ephemeral however non-zero it looks, and binding the rest of the bundle
/// with the fanout flags on let those sockets join a *stranger's* SO_REUSEPORT
/// group. The kernel then load-balances that port's datagrams between the two
/// bundles and half this client's SEARCH replies go to an unrelated process,
/// with no error anywhere. Naming the intent removes the dual meaning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortUse {
    /// A well-known port (CA 5064, PVA 5076) several processes co-bind on
    /// purpose: `SO_REUSEADDR` + `SO_REUSEPORT`, mirroring
    /// `epicsSocketEnableAddressUseForDatagramFanout`.
    Shared,
    /// A port this socket must own outright. No reuse flags, so a number
    /// somebody already holds is `EADDRINUSE` — a failure the caller can
    /// retry — instead of a silent join.
    Exclusive,
}

impl PortUse {
    /// The old `port != 0` rule, valid where the number came from the caller.
    pub fn from_caller(port: u16) -> Self {
        if port == 0 {
            Self::Exclusive
        } else {
            Self::Shared
        }
    }

    /// Apply this port's reuse options to `sock`, before its `bind`.
    ///
    /// The two options are one set, not two decisions — see [`Self::Shared`] —
    /// and applying them here is what stops a bind site from setting half of
    /// it. Half reads as working on Linux, where `SO_REUSEADDR` alone already
    /// lets a stranger co-bind a wildcard UDP port; on macOS and the BSDs an
    /// exact-duplicate datagram bind needs `SO_REUSEPORT` and `SO_REUSEADDR`
    /// alone is `EADDRINUSE`. The PVA v6 SEARCH responder set exactly that
    /// half and lost its co-bind on Darwin only.
    pub fn apply_reuse(self, sock: &socket2::Socket) -> io::Result<()> {
        if self == Self::Shared {
            sock.set_reuse_address(true)?;
            #[cfg(unix)]
            sock.set_reuse_port(true)?;
        }
        Ok(())
    }
}

/// Bind `port` on one NIC and refuse to share it. Used for the second and
/// later NICs of an ephemeral same-port bundle, where the number is non-zero
/// but the intent is still exclusive ownership.
fn bind_one_exclusive(info: &IfaceInfo, port: u16, broadcast: bool) -> io::Result<NicSocket> {
    bind_one_at(info, info.ip, port, PortUse::Exclusive, broadcast, false)
}

/// Bind to an arbitrary IPv4 address while keeping the NIC metadata
/// from `info`. Used by [`AsyncUdpV4::bind_with_map`] to create both the
/// primary unicast socket (`bind_ip = info.ip`) and an auxiliary
/// broadcast-RX socket (`bind_ip = info.broadcast`).
fn bind_one_at(
    info: &IfaceInfo,
    bind_ip: Ipv4Addr,
    port: u16,
    port_use: PortUse,
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
    // The fanout helper carries no `#ifdef _WIN32` — it sets SO_REUSEADDR
    // on every platform, Windows included. (The Windows carve-out in
    // libcom commit 19146a5 belongs to the *other* helper,
    // `epicsSocketEnableAddressReuseDuringTimeWaitState`, which guards TCP
    // time-wait rebinding; WINSOCK's SO_REUSEADDR has port-hijack
    // semantics, and C accepts that cost for datagram fanout but not for
    // TCP.) Skipping it on Windows breaks the co-bind this socket exists
    // to support.
    //
    // Only for a *well-known* port — the co-bind case the flags exist
    // for, and what C does: udpiiu.cpp:248 binds the client search socket
    // to PORT_ANY and enables no reuse on it. A socket asking for an
    // ephemeral port has nothing to share, and the flags then do harm:
    // Linux may satisfy bind(0) with a port already held by a
    // reuse-compatible socket, joining its SO_REUSEPORT group, after which
    // the kernel load-balances arriving datagrams across the group — this
    // socket's search replies can be delivered to the unrelated socket and
    // dropped. With the flags off, bind(0) yields a port this socket
    // exclusively owns.
    port_use.apply_reuse(&sock)?;
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
        // A 0.0.0.0 netmask matches every destination. Returning
        // `false` here keeps a `/0` interface from shadowing every
        // more-specific subnet in the priority-1 pass; `/0` interfaces
        // are instead matched as a fallback via [`is_default_route`].
        return false;
    }
    (u32::from(ip) & m) == (u32::from(candidate) & m)
}

/// `true` for an interface configured with a `0.0.0.0` netmask — a
/// default-route NIC. Such an interface matches every destination, so
/// it is used only as a fallback after specific subnet/broadcast
/// matches fail, never as a priority-1 match.
fn is_default_route(mask: Ipv4Addr) -> bool {
    u32::from(mask) == 0
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

    /// Boundary — a NIC joined *after* the bundle was bound
    /// ([`AsyncUdpV4::ensure_iface_socket`], which the PVA server calls for
    /// each multicast beacon destination). It is handed the bundle's resolved
    /// port, an ordinary non-zero number by then, so deriving the reuse flags
    /// from it would re-open the hole on exactly the NICs that joined last.
    /// The bundle's recorded `port_use` is what keeps them exclusive.
    #[tokio::test]
    async fn a_nic_joined_after_the_bind_inherits_the_bundle_port_use() {
        let mut sock = AsyncUdpV4::bind_ephemeral_same_port_on_interfaces(
            &[std::net::Ipv4Addr::LOCALHOST],
            false,
        )
        .expect("bind loopback-only bundle");
        let port = sock.ifaces()[0]
            .sock
            .local_addr()
            .expect("local addr")
            .port();

        // The first up non-loopback NIC, if the host has one — that is the
        // interface `join_beacon_multicast_groups` would add. With none, the
        // check reduces to re-asserting loopback's exclusivity.
        let extra = crate::net::iface_v4::enumerate()
            .unwrap_or_default()
            .into_iter()
            .find(|i| i.is_up() && !i.is_loopback() && !i.ip.is_unspecified())
            .map(|i| i.ip);
        if let Some(ip) = extra {
            sock.ensure_iface_socket(ip, port, false)
                .expect("join the NIC to the bundle");
        }

        for n in sock.ifaces() {
            let intruder =
                Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
            intruder.set_reuse_address(true).expect("reuse addr");
            #[cfg(unix)]
            intruder.set_reuse_port(true).expect("reuse port");
            let addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new(n.iface_ip, port));
            assert!(
                intruder.bind(&addr.into()).is_err(),
                "NIC {} joined the ephemeral bundle on port {port} with the fanout \
                 flags on: a stranger can co-bind it and split the datagrams",
                n.iface_ip
            );
        }
    }

    /// The bundle must OWN its ephemeral port on every NIC, not merely hold a
    /// socket there.
    ///
    /// The reuse flags used to be chosen by `port != 0`, so the first NIC —
    /// bound with `0` — got exclusive ownership while every later NIC was
    /// bound with the kernel's answer and therefore with
    /// `SO_REUSEADDR`+`SO_REUSEPORT` on. Any other process whose bundle landed
    /// on the same number joined the group on those NICs, and the kernel then
    /// load-balanced the port's datagrams between the two clients: half this
    /// client's SEARCH replies were delivered to an unrelated socket and
    /// dropped, with nothing logged on either side. This test stands in for
    /// that other process — a reuse-flagged bind that succeeds anywhere in the
    /// bundle is the defect.
    #[tokio::test]
    async fn an_ephemeral_bundle_owns_its_port_on_every_nic() {
        let sock = AsyncUdpV4::bind_ephemeral_same_port(false).expect("bind same-port");
        let nics = sock.ifaces();
        assert!(!nics.is_empty(), "at least one bound NIC");
        let port = nics[0].sock.local_addr().expect("local addr").port();

        for n in nics {
            let intruder =
                Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
            intruder.set_reuse_address(true).expect("reuse addr");
            #[cfg(unix)]
            intruder.set_reuse_port(true).expect("reuse port");
            let addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new(n.iface_ip, port));
            assert!(
                intruder.bind(&addr.into()).is_err(),
                "a stranger's fanout socket joined this bundle's port {port} on {}: \
                 the bundle does not own it, and the kernel will split its \
                 SEARCH replies between the two",
                n.iface_ip
            );
        }
    }

    /// The invariant the retry ladder exists to preserve: one socket per
    /// selected NIC. A bundle short of a NIC searches on fewer interfaces than
    /// the caller believes, so a server reachable only on the missing one is
    /// never found — and the operation expires against whatever timeout
    /// surrounds it rather than reporting a bind problem.
    #[tokio::test]
    async fn a_bundle_binds_one_socket_per_selected_nic() {
        let map = IfaceMap::new().expect("iface map");
        let expected = select_ifaces(&map, None).len();
        let sock = AsyncUdpV4::bind_ephemeral_same_port(false).expect("bind same-port");
        assert_eq!(
            sock.ifaces().len(),
            expected,
            "bundle covers {} of {expected} NICs",
            sock.ifaces().len()
        );
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
    async fn bind_on_interfaces_loopback_only_binds_no_other_nic() {
        // EPICS_PVA*_INTF_ADDR_LIST=127.0.0.1 must restrict the bundle
        // to loopback — no non-loopback NIC may be bound.
        let sock = AsyncUdpV4::bind_on_interfaces(&[Ipv4Addr::LOCALHOST], 0, true)
            .expect("loopback-only bind");
        assert!(
            sock.ifaces().iter().all(|n| n.is_loopback),
            "loopback-only interface list bound a non-loopback NIC: {:?}",
            sock.ifaces().iter().map(|n| n.iface_ip).collect::<Vec<_>>()
        );
        assert!(
            sock.local_addrs().iter().all(|a| a.ip().is_loopback()),
            "loopback-only interface list produced a non-loopback bind addr"
        );
    }

    #[tokio::test]
    async fn bind_ephemeral_same_port_on_interfaces_loopback_only() {
        // Client active-search socket constrained to loopback: one
        // socket, loopback only, no non-loopback egress interface.
        let sock = AsyncUdpV4::bind_ephemeral_same_port_on_interfaces(&[Ipv4Addr::LOCALHOST], true)
            .expect("loopback-only ephemeral bind");
        assert!(
            sock.ifaces().iter().all(|n| n.is_loopback),
            "loopback-only search socket bound a non-loopback NIC: {:?}",
            sock.ifaces().iter().map(|n| n.iface_ip).collect::<Vec<_>>()
        );
        let addrs = sock.local_addrs();
        assert!(!addrs.is_empty(), "must bind at least the loopback socket");
        assert!(
            addrs.iter().all(|a| a.ip().is_loopback()),
            "loopback-only search socket produced a non-loopback bind addr"
        );
    }

    #[tokio::test]
    async fn join_multicast_v4_on_absent_iface_is_not_found() {
        // `join_multicast_v4_on` joins one interface only; when no bundle
        // socket is bound to that interface it must surface NotFound so the
        // caller (server) can `ensure_iface_socket` first.
        let sock = AsyncUdpV4::bind_on_interfaces(&[Ipv4Addr::LOCALHOST], 0, true)
            .expect("loopback-only bind");
        let group = Ipv4Addr::new(224, 0, 9, 9);
        let absent = Ipv4Addr::new(203, 0, 113, 77);
        let err = sock
            .join_multicast_v4_on(group, absent)
            .expect_err("absent interface must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn ensure_iface_socket_adds_then_is_idempotent() {
        // A bundle constrained to loopback (as under
        // `EPICS_PVAS_INTF_ADDR_LIST=127.0.0.1`) has no external NIC
        // socket. `ensure_iface_socket` must add one on demand (the pvxs
        // `addGroups` "listener interface outside the configured set"
        // behaviour) and be a no-op when the interface is already present.
        let mut sock = AsyncUdpV4::bind_on_interfaces(&[Ipv4Addr::LOCALHOST], 0, true)
            .expect("loopback-only bind");
        assert!(
            !sock
                .ensure_iface_socket(Ipv4Addr::LOCALHOST, 0, true)
                .expect("loopback already present"),
            "loopback socket already in the bundle must not be re-added"
        );

        let before = sock.ifaces().len();
        // Add a real external NIC (bindable, but outside the loopback
        // bundle). An empty list is now the host's answer and nothing else
        // — a failed enumeration errors out of `IfaceMap::new` instead of
        // arriving here as "no NIC" — so the skip states a host fact.
        let Some(nic) = IfaceMap::new()
            .expect("getifaddrs must succeed on the host")
            .up_non_loopback()
            .into_iter()
            .next()
        else {
            eprintln!("skipping: no external NIC here, so the add branch has no input");
            return;
        };
        assert!(
            sock.ensure_iface_socket(nic.ip, 0, true)
                .expect("bind external NIC"),
            "an external NIC outside the bundle must be added"
        );
        assert_eq!(
            sock.ifaces().len(),
            before + 1,
            "exactly one listener socket appended"
        );
        assert!(
            !sock
                .ensure_iface_socket(nic.ip, 0, true)
                .expect("now present"),
            "second call for the same interface is a no-op"
        );
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
    async fn send_multicast_v4_unknown_iface_returns_addr_not_available() {
        // An `@iface` that matches no bound NIC selects nothing → error,
        // never a silent no-op (mirrors `send_via`'s unknown-iface contract).
        let sock = AsyncUdpV4::bind(0, true).expect("bind");
        let bogus = Ipv4Addr::new(203, 0, 113, 99);
        let group = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 1, 2, 3), 9999));
        let err = sock
            .send_multicast_v4(b"x", group, 1, Some(bogus))
            .await
            .expect_err("unknown iface must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[tokio::test]
    async fn send_multicast_v4_no_iface_matches_fanout_nic_set() {
        // `iface_ip == None` must target exactly the eligible NIC set that the
        // proven `fanout_to` uses (non-loopback, non-rx-only) — adding only the
        // per-NIC TTL setsockopt, which never changes which NICs are reached.
        // Comparing the two best-effort counts is robust to a host NIC that
        // can't route the group (both paths skip the same failures); asserting
        // an absolute count would be flaky. On a loopback-only host both error.
        let sock = AsyncUdpV4::bind(0, true).expect("bind");
        let group = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 1, 2, 3), 9999));
        let mc = sock.send_multicast_v4(b"x", group, 1, None).await;
        let fo = sock.fanout_to(b"x", group).await;
        match (mc, fo) {
            (Ok(a), Ok(b)) => assert_eq!(
                a, b,
                "send_multicast_v4(None) must reach the same NIC set as fanout_to"
            ),
            (Err(_), Err(_)) => {} // loopback-only host: no eligible NIC either way
            (a, b) => {
                panic!("send_multicast_v4 and fanout_to disagreed on eligibility: {a:?} vs {b:?}")
            }
        }
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

    /// L3 C-parity: a `0.0.0.0` netmask identifies a default-route
    /// interface — `subnet_contains` rejects it (so it never shadows a
    /// specific subnet) but `is_default_route` flags it for the
    /// `pick_nic` fallback pass.
    #[test]
    fn default_route_iface_is_recognised() {
        assert!(is_default_route(Ipv4Addr::UNSPECIFIED));
        assert!(!is_default_route(Ipv4Addr::new(255, 255, 255, 0)));
        assert!(!is_default_route(Ipv4Addr::new(255, 0, 0, 0)));
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

    /// Multi-NIC variant: `recv_with_meta_with_drops` must round-trip
    /// `RecvMeta` (n / src / iface_ip) identically to the no-drops
    /// `recv_with_meta` and report `drop_count = 0` under normal load.
    #[tokio::test]
    async fn recv_with_meta_with_drops_returns_zero_under_normal_load() {
        let server = AsyncUdpV4::bind(0, false).expect("server bind");
        server.enable_so_rxq_ovfl().expect("enable counter");
        // Send to the server's LOOPBACK socket specifically. `bind(0,..)`
        // binds one socket per NIC on independent ephemeral ports, and the
        // loopback socket is not guaranteed to be index 0 — NIC enumeration
        // order differs across platforms (it is last on Windows), so a send
        // to `sockets[0]`'s port over 127.0.0.1 would never arrive there and
        // the recv timed out. Select by `is_loopback`, as `loopback_send_and_recv`.
        let dest = server
            .ifaces()
            .iter()
            .find(|n| n.is_loopback)
            .map(|n| n.sock.local_addr().unwrap())
            .expect("loopback NIC must exist");

        let client = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("client bind");
        let payload = b"meta-with-drops-payload";
        client.send_to(payload, dest).await.expect("send");

        let mut buf = [0u8; 64];
        let (meta, drops) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.recv_with_meta_with_drops(&mut buf),
        )
        .await
        .expect("recv timeout")
        .expect("recv ok");
        assert_eq!(meta.n, payload.len(), "byte count must match");
        assert_eq!(&buf[..meta.n], payload, "payload must round-trip");
        assert!(meta.iface_ip.is_loopback(), "loopback recv path expected");
        assert_eq!(drops, 0, "freshly-bound socket must report 0 drops");
    }

    /// `recv_from_with_drop_count` returns `drop_count = 0` on a
    /// freshly-bound socket whose receive buffer never overflowed.
    /// The src/n payload must round-trip through the recvmsg path
    /// identically to `recv_from`.
    #[tokio::test]
    async fn recv_from_with_drop_count_returns_zero_under_normal_load() {
        // `recv_from_with_drop_count` reads `sockets.first()` only — its
        // documented single-binding contract. Bind a single loopback socket
        // so the socket the method reads IS the one we send to on every
        // platform. Multi-NIC `bind()` places the loopback at an
        // enumeration-order-dependent index (first on Linux/macOS, not first
        // on Windows); sending to the loopback NIC while the method reads
        // `sockets[0]` would then never deliver off those platforms. (The
        // fan-in `recv_with_meta_with_drops` sibling can send to loopback
        // because it receives across all sockets.)
        let server = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("server bind");
        server.enable_so_rxq_ovfl().expect("enable counter");
        let dest = server.ifaces()[0].sock.local_addr().expect("loopback addr");

        let client = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("client bind");
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
