//! Server beacons, pvxs `Server::Pvt::doBeacons` (`server.cpp:751-820`): the
//! destination list, the per-beacon state and the frame. The tokio responder's
//! beacon task and the native drivers' `PVAS-beacon` thread drive the same
//! [`BeaconCycle`], so the two backends put the same stream on the wire.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use epics_base_rs::net::{McastSocket, mcast_prep_v4, mcast_prep_v6};
use tracing::{debug, warn};

use crate::config::Endpoint;
use crate::proto::{ByteOrder, Command, PvaHeader, WriteExt, encode_string_into, ip_to_bytes};

/// Every beacon destination: the explicit list, plus under `auto_beacon` the
/// broadcasts of [`ServerInterfaces::bcast_ifaces`] (pvxs `Config::expand`,
/// `config.cpp:488-516`), deduplicated last (`removeDups`, `config.cpp:523`)
/// so the synthesized entries are too. pvxs synthesizes no v6 destination.
/// Without `auto_beacon` an empty list beacons nowhere, as pvxs's does. The
/// one owner of the list the servers beacon to and `pvxsr` prints.
///
/// The expanded broadcasts carry port 0 through the dedup and take
/// `udp_port` after it (`expandAddrList` `setPort(0u)`, `config.cpp:304`,
/// resolved at `server.cpp:491`), so an explicit `x.255:5076` and the
/// auto `x.255` both stay, and pvxs sends that broadcast twice.
///
/// [`ServerInterfaces::bcast_ifaces`]: super::search_engine::ServerInterfaces::bcast_ifaces
pub(crate) fn beacon_destinations(
    explicit: Vec<Endpoint>,
    auto_beacon: bool,
    bcast_ifaces: &[Ipv4Addr],
    udp_port: u16,
) -> Vec<Endpoint> {
    let mut dests = explicit;
    if auto_beacon {
        dests.extend(
            crate::config::env::expand_addr_list(bcast_ifaces, 0)
                .into_iter()
                .map(Endpoint::from),
        );
    }
    let mut dests = crate::config::env::dedup_endpoints(dests);
    for ep in &mut dests {
        if ep.addr.port() == 0 {
            ep.addr.set_port(udp_port);
        }
    }
    dests
}

/// One server's beacon stream between two sends.
pub(crate) struct BeaconCycle {
    seq: u8,
    emitted: u32,
    send_errs: HashSet<SocketAddr>,
    period: Duration,
    period_long: Duration,
    burst: u8,
}

impl BeaconCycle {
    pub(crate) fn new(period: Duration, period_long: Duration, burst: u8) -> Self {
        Self {
            seq: 0,
            emitted: 0,
            send_errs: HashSet::new(),
            period,
            period_long,
            burst,
        }
    }

    /// The beacon to send now; advances the sequence. Its change count is
    /// `beacon_change` truncated, as pvxs writes `uint16_t(beaconChange)`, the
    /// count of every source and PV add/remove since construction
    /// (`server.cpp:761`).
    pub(crate) fn frame(
        &mut self,
        beacon_change: u64,
        guid: [u8; 12],
        tcp_port: u16,
        protocol: &str,
    ) -> Vec<u8> {
        let beacon = build_beacon(guid, tcp_port, self.seq, beacon_change as u16, protocol);
        self.seq = self.seq.wrapping_add(1);
        self.emitted = self.emitted.saturating_add(1);
        beacon
    }

    /// Record one destination's send; a failure warns once per outage.
    pub(crate) fn sent(&mut self, dest: SocketAddr, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                self.send_errs.remove(&dest);
            }
            Err(e) => {
                if self.send_errs.insert(dest) {
                    warn!("beacon TX to {dest} failed: {e}");
                } else {
                    debug!("beacon TX to {dest} failed: {e}");
                }
            }
        }
    }

    /// The wait before the next beacon: `beacon_period` for the first
    /// `beacon_burst_count` waits, then `beacon_period_long`. pvxs reads
    /// `beaconCnt` before its increment (`server.cpp:813-817`), so its 10
    /// short waits put 11 beacons out at the short period.
    pub(crate) fn period(&self) -> Duration {
        if self.emitted <= self.burst as u32 {
            self.period
        } else {
            self.period_long
        }
    }
}

/// Big-endian, as pvxs `doBeacons` builds it (`VectorOutBuf(true)`,
/// `server.cpp:755`).
pub(crate) fn build_beacon(
    guid: [u8; 12],
    tcp_port: u16,
    sequence: u8,
    change_count: u16,
    protocol: &str,
) -> Vec<u8> {
    let order = ByteOrder::Big;
    let mut payload = Vec::new();
    payload.extend_from_slice(&guid);
    // pvxs server.cpp::doBeacons: flags(u8) + seq(u8) + change(u16) = 4 bytes
    payload.put_u8(0); // flags / QoS (undefined, 0)
    payload.put_u8(sequence);
    payload.put_u16(change_count, order);
    let addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    payload.extend_from_slice(&addr);
    payload.put_u16(tcp_port, order);
    // beacons advertise the runtime transport. Pre-fix
    // Rust hard-coded "tcp" even when SEARCH_RESPONSE said "tls",
    // making discovery inconsistent across the two announcement
    // channels. pvxs `src/server.cpp:727, 765` keeps both
    // consistent.
    encode_string_into(protocol, order, &mut payload);
    payload.put_u8(0xFF); // null serverStatus marker (matches pvxs)
    let header = PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// pvxs `beaconSender4`: unbound until the first send, broadcast on.
pub(crate) fn beacon_socket_v4() -> std::io::Result<std::net::UdpSocket> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    Ok(sock)
}

/// pvxs `evsocket::mcast_prep_sendto` (`evhelper.cpp:573-609`) before a
/// beacon to `dest`: a multicast group takes the endpoint's TTL and egress
/// interface, anything else is left. pvxs logs a failed option and sends
/// anyway, so this reports nothing to the caller.
pub(crate) fn mcast_prep(
    sock: &impl McastSocket,
    ep: &Endpoint,
    dest: SocketAddr,
    ifmap: &crate::config::IfaceMap,
) {
    if !dest.ip().is_multicast() {
        return;
    }
    let result = match dest {
        SocketAddr::V4(_) => mcast_prep_v4(sock, ep.mcast_ttl(), ep.mcast_iface_v4(ifmap)),
        SocketAddr::V6(_) => mcast_prep_v6(sock, ep.mcast_ttl(), ep.mcast_iface_v6(ifmap)),
    };
    if let Err(e) = result {
        debug!(%dest, error = %e, "beacon multicast TTL/IF prep failed");
    }
}

/// pvxs `beaconSender6`: a v6-only ephemeral socket for the v6 groups, or
/// `None` where the host has no v6 (the beacons stay v4-only).
#[cfg(not(epics_embedded_target))]
pub(crate) fn beacon_socket_v6() -> Option<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let sock = match Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            debug!("v6 beacon socket: socket() failed: {e}; v6 beacon disabled");
            return None;
        }
    };
    if let Err(e) = sock.set_only_v6(true) {
        debug!("v6 beacon socket: set_only_v6 failed: {e}");
    }
    let bind = SocketAddr::V6(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    ));
    if let Err(e) = sock.bind(&bind.into()) {
        debug!("v6 beacon socket: bind {bind} failed: {e}; v6 beacon disabled");
        return None;
    }
    Some(sock.into())
}

#[cfg(epics_embedded_target)]
pub(crate) fn beacon_socket_v6() -> Option<std::net::UdpSocket> {
    None
}

/// The error a v6 destination gets with no v6 send socket.
pub(crate) fn no_v6_socket() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        "v6 beacon destination configured without a v6 send socket",
    )
}

/// A native server's beacon stream, set up as pvxs's `Server` constructor
/// sets up its senders and destination list (`server.cpp:480-496`), so a
/// failure is the bind's, not the beacon thread's.
pub struct BeaconSender {
    source: super::source::DynSource,
    dests: Vec<Endpoint>,
    v4: std::net::UdpSocket,
    v6: Option<std::net::UdpSocket>,
    cycle: BeaconCycle,
    guid: [u8; 12],
    tcp_port: u16,
}

impl BeaconSender {
    /// Beacons for `config`'s bound server, the auto destinations on the
    /// search port `udp_port`.
    pub fn bind(
        source: super::source::DynSource,
        config: &super::config::PvaServerConfig,
        udp_port: u16,
    ) -> std::io::Result<Self> {
        let interfaces = super::search_engine::ServerInterfaces::resolve(
            &config.interfaces,
            &config.beacon_destinations,
            config.auto_beacon,
        )?;
        let dests = beacon_destinations(
            config.beacon_destinations.clone(),
            config.auto_beacon,
            &interfaces.bcast_ifaces(),
            udp_port,
        );
        Ok(Self {
            source,
            v4: beacon_socket_v4()?,
            v6: if dests.iter().any(|e| e.addr.is_ipv6()) {
                beacon_socket_v6()
            } else {
                None
            },
            dests,
            cycle: BeaconCycle::new(
                config.beacon_period,
                config.beacon_period_long,
                config.beacon_burst_count,
            ),
            guid: config.guid,
            tcp_port: config.tcp_port,
        })
    }

    /// The stream on the calling thread, first beacon at once as pvxs arms
    /// its timer `immediate` (`server.cpp:590-592`), until `sleep(period)`
    /// returns `true` because serving ended.
    pub(crate) fn run(mut self, sleep: &mut dyn FnMut(Duration) -> bool) {
        // pvxs sends from the acceptor loop, `PVXTCP` (`server.cpp:388-392`).
        let _ =
            epics_base_rs::runtime::task::enter_ioc_thread(super::blocking::PVA_SERVER_PRIORITY);
        loop {
            let beacon =
                self.cycle
                    .frame(self.source.beacon_change(), self.guid, self.tcp_port, "tcp");
            let ifmap = crate::config::IfaceMap::for_endpoints(self.dests.iter());
            for ep in &self.dests {
                let result = match ep.addr {
                    SocketAddr::V4(_) => {
                        mcast_prep(&self.v4, ep, ep.addr, &ifmap);
                        self.v4.send_to(&beacon, ep.addr).map(|_| ())
                    }
                    SocketAddr::V6(_) => match &self.v6 {
                        Some(s6) => {
                            mcast_prep(s6, ep, ep.addr, &ifmap);
                            s6.send_to(&beacon, ep.addr).map(|_| ())
                        }
                        None => Err(no_v6_socket()),
                    },
                };
                self.cycle.sent(ep.addr, result);
            }
            if sleep(self.cycle.period()) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The burst runs at `beacon_period`, then the long period; the
    /// sequence advances by one per frame and wraps.
    #[test]
    fn the_cycle_bursts_then_slows_and_counts() {
        let (short, long) = (Duration::from_secs(15), Duration::from_secs(180));
        let mut cycle = BeaconCycle::new(short, long, 2);
        let seq = |b: &[u8]| b[8 + 12 + 1];
        let mut periods = Vec::new();
        for want in 0..3u8 {
            let b = cycle.frame(0, [0; 12], 5075, "tcp");
            assert_eq!(seq(&b), want);
            periods.push(cycle.period());
        }
        assert_eq!(periods, [short, short, long], "two short waits, then long");
    }

    /// An explicit broadcast and the same broadcast from the auto list are
    /// two destinations, as in pvxs; the auto one takes the UDP port.
    #[test]
    fn an_explicit_broadcast_and_its_auto_twin_both_stay() {
        let Some(bcast) = epics_base_rs::net::iface_v4::enumerate()
            .unwrap_or_default()
            .iter()
            .find_map(|i| i.search_destination().filter(|b| !b.is_loopback()))
        else {
            return;
        };
        let addr = SocketAddr::from((bcast, 5076));
        let dests = beacon_destinations(vec![addr.into()], true, &[Ipv4Addr::UNSPECIFIED], 5076);
        let to_bcast = dests.iter().filter(|e| e.addr == addr).count();
        assert_eq!(to_bcast, 2, "{dests:?}");
        assert!(dests.iter().all(|e| e.addr.port() == 5076), "{dests:?}");
    }

    /// The frame softIocPVX (pvxs 1.5.3) sent as its second beacon to
    /// 127.0.0.1:48412, byte for byte.
    #[test]
    fn the_beacon_is_the_pvxs_frame() {
        let pvxs = "ca02c000000000273b997f141c488a354c6867c10001000200000000\
                    000000000000ffff00000000bd1a03746370ff";
        let pvxs: Vec<u8> = (0..pvxs.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&pvxs[i..i + 2], 16).unwrap())
            .collect();
        let guid: [u8; 12] = pvxs[8..20].try_into().unwrap();
        assert_eq!(build_beacon(guid, 48410, 1, 2, "tcp"), pvxs);
    }

    /// The change count is the source's count itself, not one relative to
    /// the first beacon: softIocPVX's first beacon reads 2.
    #[test]
    fn the_change_count_is_the_absolute_beacon_change() {
        let mut cycle = BeaconCycle::new(Duration::ZERO, Duration::ZERO, 0);
        let change = |b: &[u8]| u16::from_be_bytes([b[8 + 14], b[8 + 15]]);
        assert_eq!(change(&cycle.frame(2, [0; 12], 1, "tcp")), 2);
        assert_eq!(change(&cycle.frame(0x1_0005, [0; 12], 1, "tcp")), 5);
    }

    /// A v6 group gets the endpoint's TTL as hops and its interface's
    /// index, as pvxs `mcast_prep_sendto` sets them; a unicast one is left.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_v6_group_destination_takes_its_ttl_and_interface() {
        let Some(sock) = beacon_socket_v6() else {
            return;
        };
        let group = std::net::SocketAddrV6::new("ff02::1".parse().unwrap(), 5076, 0, 0);
        let ep = Endpoint {
            addr: SocketAddr::V6(group),
            ttl: Some(4),
            iface: Some("lo".to_string()),
        };
        let sref = socket2::SockRef::from(&sock);
        mcast_prep(&sock, &ep, ep.addr, &crate::config::IfaceMap::current());
        assert_eq!(sref.multicast_hops_v6().unwrap(), 4);
        assert_eq!(
            sref.multicast_if_v6().unwrap(),
            crate::config::IfaceMap::current().index_of("lo")
        );

        let unicast = std::net::SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, 5076, 0, 0);
        let ep = Endpoint {
            addr: SocketAddr::V6(unicast),
            ttl: Some(9),
            iface: None,
        };
        mcast_prep(&sock, &ep, ep.addr, &crate::config::IfaceMap::current());
        assert_eq!(sref.multicast_hops_v6().unwrap(), 4);
    }
}
