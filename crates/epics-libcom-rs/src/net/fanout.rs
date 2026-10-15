//! When a datagram-fanout socket takes `SO_REUSEADDR`/`SO_REUSEPORT`
//! relative to its `bind` — the one rule every fanout bind goes through,
//! whichever socket API (socket2 or raw `libc`) performs it.

use std::io;

/// The socket's role, stated by the caller; never read off the port number.
///
/// pvxs and libca set the reuse options by role, port 0 or not: every pvxs
/// `UDPCollector` socket gets `epicsSocketEnableAddressUseForDatagramFanout`
/// (`udp_collector.cpp:136`), while the pvxs client search socket
/// (`client.cpp:578-590`) and libca's `udpiiu` socket (`udpiiu.cpp:174-252`)
/// bind port 0 with none. A rule keyed on `port != 0` gets the PVA server on
/// an ephemeral port wrong: a pvxs client told that port as its broadcast
/// port binds its beacon receiver there with fanout, and an exclusive bundle
/// makes that bind `EADDRINUSE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortUse {
    /// A `UDPCollector` socket — the PVA server search responder and the PVA
    /// client beacon listener — on any port, 0 included: `SO_REUSEADDR` +
    /// `SO_REUSEPORT`, as `epicsSocketEnableAddressUseForDatagramFanout`.
    /// On a number of its own choosing they are set after the bind, see
    /// [`bind_fanout`].
    Shared,
    /// The PVA client search socket and the CA client sockets: no reuse
    /// options, so a number somebody already holds is `EADDRINUSE` — a
    /// failure the caller can retry — instead of a silent join.
    Exclusive,
}

#[cfg(not(any(target_os = "rtems", target_os = "vxworks")))]
impl PortUse {
    /// Bind `sock` on `addr` with this role's reuse options, ordered against
    /// the bind by [`bind_fanout`].
    ///
    /// The two options are one set, not two decisions — see [`Self::Shared`] —
    /// and applying them only here is what stops a bind site from setting half
    /// of it. Half reads as working on Linux, where `SO_REUSEADDR` alone already
    /// lets a stranger co-bind a wildcard UDP port; on macOS and the BSDs an
    /// exact-duplicate datagram bind needs `SO_REUSEPORT` and `SO_REUSEADDR`
    /// alone is `EADDRINUSE`. The PVA v6 SEARCH responder set exactly that
    /// half and lost its co-bind on Darwin only.
    pub fn bind(
        self,
        sock: &socket2::Socket,
        addr: std::net::SocketAddr,
        origin: PortOrigin,
    ) -> io::Result<()> {
        let bind = || sock.bind(&addr.into());
        match self {
            Self::Shared => bind_fanout(
                addr.port(),
                origin,
                || {
                    sock.set_reuse_address(true)?;
                    #[cfg(unix)]
                    sock.set_reuse_port(true)?;
                    Ok(())
                },
                bind,
            ),
            Self::Exclusive => bind(),
        }
    }
}

/// Whether a refused TCP listener bind falls back to a dynamic port, as pvxs
/// does on EADDRINUSE or EACCES on every platform (`serverconn.cpp:437`):
/// the number cannot be had here, whoever or whatever refuses it.
pub fn bind_falls_back(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// Whether a refused bind says another socket holds the number: EADDRINUSE,
/// and on Windows `WSAEACCES` too, which Winsock returns for a held number
/// whenever either socket set `SO_REUSEADDR` or `SO_EXCLUSIVEADDRUSE`. A unix
/// EACCES is a privileged port or a policy, never a holder.
pub fn port_held(e: &io::Error) -> bool {
    /// Winsock's `WSAEACCES`.
    #[cfg(windows)]
    const WSAEACCES: i32 = 10013;
    #[cfg(windows)]
    if e.raw_os_error() == Some(WSAEACCES) {
        return true;
    }
    e.kind() == io::ErrorKind::AddrInUse
}

/// Whose number a fanout socket binds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortOrigin {
    /// A number other fanout sockets on the host may already share: the
    /// configured port, or one this process already holds. Port 0 is
    /// [`Self::Chosen`] whatever the caller says.
    Configured,
    /// A number this process picks — `bind(0)`, or the number a same-port
    /// bundle took on its first NIC — which nobody else may hold.
    Chosen,
}

/// Bind a fanout socket on `port`: `set_fanout` applies the reuse options,
/// `bind` performs the bind.
///
/// A chosen number is bound before the options are set. With them set first,
/// Linux treats a number that another same-user fanout socket holds as free,
/// hands it to `bind(0)` and joins that socket's `SO_REUSEPORT` group, which
/// then load-balances unicasts between the two. Set after the bind they still
/// let a later fanout socket — a pvxs client's beacon receiver — co-bind.
/// pvxs sets them first on every port (`udp_collector.cpp:136`); deliberate
/// deviation on unix only, see `docs/differences.md`. Winsock reads
/// `SO_REUSEADDR` at bind time, so set after it the socket refuses every
/// co-binder with WSAEACCES; Windows sets them first on every port, as C.
pub fn bind_fanout(
    port: u16,
    origin: PortOrigin,
    mut set_fanout: impl FnMut() -> io::Result<()>,
    bind: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let shared_number = cfg!(windows) || (origin == PortOrigin::Configured && port != 0);
    if shared_number {
        set_fanout()?;
    }
    bind()?;
    if !shared_number {
        set_fanout()?;
    }
    Ok(())
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    /// An access refusal is a holder's answer only from Winsock; a unix EACCES
    /// is a privileged port, which only the fallback reads as unavailable.
    #[test]
    fn an_access_refusal_is_held_only_on_windows() {
        let in_use = io::Error::from(io::ErrorKind::AddrInUse);
        assert!(port_held(&in_use) && bind_falls_back(&in_use));
        #[cfg(windows)]
        let refused = io::Error::from_raw_os_error(10013); // WSAEACCES
        #[cfg(unix)]
        let refused = io::Error::from_raw_os_error(13); // EACCES
        assert!(bind_falls_back(&refused));
        assert_eq!(port_held(&refused), cfg!(windows));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, Socket, Type};
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::time::Duration;

    const ANY_0: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

    fn udp() -> Socket {
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap()
    }

    /// Same-user fanout sockets on kernel-chosen numbers, the options set
    /// before the bind as pvxs does — the strangers a test run is full of.
    fn fanout_strangers(n: usize) -> (Vec<Socket>, HashSet<u16>) {
        let mut held = Vec::with_capacity(n);
        let mut ports = HashSet::with_capacity(n);
        for _ in 0..n {
            let s = udp();
            s.set_reuse_address(true).unwrap();
            s.set_reuse_port(true).unwrap();
            s.bind(&ANY_0.into()).unwrap();
            ports.insert(s.local_addr().unwrap().as_socket().unwrap().port());
            held.push(s);
        }
        (held, ports)
    }

    /// With the options set first, Linux hands such a bind a number one of
    /// 2000 strangers holds about one time in fifteen, and the datagram then
    /// goes to the stranger about half the time.
    #[test]
    fn a_chosen_number_is_never_a_strangers() {
        let (_held, theirs) = fanout_strangers(2000);
        for _ in 0..200 {
            let s = udp();
            PortUse::Shared
                .bind(&s, ANY_0, PortOrigin::Configured)
                .unwrap();
            let s: UdpSocket = s.into();
            let port = s.local_addr().unwrap().port();
            assert!(!theirs.contains(&port), "bound {port}, a stranger's");
            let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            sender
                .send_to(b"ours", (Ipv4Addr::LOCALHOST, port))
                .unwrap();
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 4];
            assert_eq!(s.recv(&mut buf).expect("the datagram is ours"), 4);
        }
    }

    /// The options still go on: a pvxs client's beacon receiver co-binds.
    #[test]
    fn a_chosen_number_lets_a_fanout_listener_co_bind() {
        let first = udp();
        PortUse::Shared
            .bind(&first, ANY_0, PortOrigin::Chosen)
            .unwrap();
        let addr = first.local_addr().unwrap().as_socket().unwrap();
        let second = udp();
        PortUse::Shared
            .bind(&second, addr, PortOrigin::Configured)
            .expect("fanout co-bind");
    }

    /// A configured number joins the fanout socket already on it, as two
    /// servers on 5076 do.
    #[test]
    fn a_configured_number_joins_its_holder() {
        let (held, _) = fanout_strangers(1);
        let addr = held[0].local_addr().unwrap().as_socket().unwrap();
        let s = udp();
        PortUse::Shared
            .bind(&s, addr, PortOrigin::Configured)
            .expect("join the holder");
    }

    /// A chosen non-zero number — a bundle's later NIC — is refused when a
    /// fanout socket holds it, so the bundle picks again.
    #[test]
    fn a_chosen_number_held_by_a_fanout_socket_is_refused() {
        let (held, _) = fanout_strangers(1);
        let addr = held[0].local_addr().unwrap().as_socket().unwrap();
        let err = PortUse::Shared
            .bind(&udp(), addr, PortOrigin::Chosen)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }
}
