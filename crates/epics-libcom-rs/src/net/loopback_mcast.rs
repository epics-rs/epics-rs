//! Loopback IPv4 multicast socket — pvxs CMD_ORIGIN_TAG forwarding channel.
//!
//! pvxs `UDPCollector` (udp_collector.cpp:127) opens its UDP socket
//! wildcard-bound and joins `224.0.0.128` on the loopback interface so
//! that any local PVA peer (including itself) can forward "originated"
//! packets back to all local listeners. The destination IP of the
//! original packet is preserved in a CMD_ORIGIN_TAG prefix; on receive
//! the listener peels it and processes the inner SEARCH/etc. packet
//! as if it had arrived directly.
//!
//! Why a separate socket from [`super::AsyncUdpV4`]? pvxs collapses
//! the SEARCH receive path and the ORIGIN_TAG receive path into a
//! single wildcard socket. Our per-NIC bundle binds the loopback NIC
//! to `127.0.0.1` (not wildcard), which the kernel will not deliver
//! `224.0.0.128`-destined packets to. Splitting it lets us keep the
//! per-NIC SEARCH path tight while still fielding ORIGIN_TAG forwards.
//!
//! Reference: pvxs `udp_collector.cpp:140-167` (bind + mcast join),
//! `udp_collector.cpp:561-567` (forward send), `evhelper.cpp:519-592`
//! (the mcast option setters: `mcast_join`, `mcast_leave`, `mcast_prep_sendto`).
//!
//! PIN-ONLY: the `evhelper.cpp` span resolves at the pvxs pin `b568e93`
//! (1.5.1-42), where `mcast_join` opens at `:519`. It does NOT resolve at
//! this machine's pvxs HEAD `bd2243d` (1.5.2-26), which adds five lines
//! ahead of it — there the same three are `:524-597`, with `mcast_join` at
//! `:524`, `mcast_leave` `:547`, `mcast_prep_sendto` `:561`, and `:519` an
//! `#endif`. A repin to HEAD must add 5; leaving the number is what makes it
//! wrong.

// RTEMS-EXEC-MODEL-ALLOW(3): every test binds tokio::net UDP sockets, which
// need a reactor the exec backend does not start; on the exec backend these
// still run on the tokio driver `#[tokio::test]` builds, and pass.
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

// The group literal itself is wire data, not a socket, and lives in
// [`super::ORIGIN_TAG_MCAST_GROUP`] so RTEMS builds — which drop this
// module — can still name it.
pub use super::ORIGIN_TAG_MCAST_GROUP;

/// Bind a UDP socket configured for the pvxs ORIGIN_TAG forwarding
/// channel.
///
/// The socket is:
/// - bound to `0.0.0.0:port` (wildcard — pvxs convention; binding to
///   the multicast address itself is not portable across Linux/macOS/
///   Windows per pvxs `udp_collector.cpp:140-149`),
/// - SO_REUSEADDR + SO_REUSEPORT (Unix) so multiple PVA processes on
///   one host can co-bind the SEARCH port,
/// - joined to `224.0.0.128` via the loopback interface (`127.0.0.1`)
///   so forwarded packets are delivered,
/// - configured with `IP_MULTICAST_LOOP=1`, `IP_MULTICAST_TTL=1`, and
///   `IP_MULTICAST_IF=127.0.0.1` so any send to `224.0.0.128` from
///   this socket loops back into local listeners and never escapes
///   the host.
///
/// Returns the tokio-wrapped socket. Caller still drives `recv_from`/
/// `send_to` on it; this helper does no I/O.
pub fn bind_loopback_mcast(port: u16) -> io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    // Same SO_REUSE* policy as the per-NIC bundle: required so a PVA
    // server and client on the same host can both join the group.
    //
    // On every platform, Windows included: this is the socket pvxs builds
    // in `UDPCollector` (udp_collector.cpp:124-151), and it hands it to
    // `epicsSocketEnableAddressUseForDatagramFanout` (:136) — the libcom
    // helper that sets SO_REUSEPORT then SO_REUSEADDR with no `#ifdef
    // _WIN32`. (The Windows carve-out of commit 19146a5 belongs to the TCP
    // time-wait helper, `epicsSocketEnableAddressReuseDuringTimeWaitState`.
    // Applying it here would stop a second local PVA process from joining
    // the ORIGIN_TAG channel on Windows.)
    //
    // Set unconditionally, including for an ephemeral port — unlike the
    // unicast sockets in `async_udp_v4`. Co-binding *is* the purpose of
    // this helper (one listener takes an ephemeral port and publishes it,
    // the others join that same port), and multicast group traffic is
    // delivered to every joined socket rather than load-balanced across the
    // reuse group, so sharing costs nothing here.
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;

    // Linux: don't let mcast packets that arrive on a different NIC
    // bleed into this socket. Mirrors libcom 51191e6155 — the kernel
    // default IP_MULTICAST_ALL=1 is wrong for our per-NIC routing.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }

    sock.set_nonblocking(true)?;
    let bind_addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
    sock.bind(&bind_addr.into())?;

    // Receive side: join 224.0.0.128 on the loopback interface so the
    // kernel delivers forwarded packets to us.
    sock.join_multicast_v4(&ORIGIN_TAG_MCAST_GROUP, &Ipv4Addr::LOCALHOST)?;

    // Send side: any send to 224.0.0.128 from this socket goes out
    // via loopback (TTL=1 so it stays on this host) and loops back
    // into our own and any peer's listeners.
    //
    // TTL is intentionally hard-coded to 1 and does NOT honour
    // `runtime::net::ca_mcast_ttl()` / `EPICS_CA_MCAST_TTL`: this is
    // the host-local pvxs ORIGIN_TAG channel (udp_collector.cpp),
    // which is TTL=1 by design. `EPICS_CA_MCAST_TTL` governs routed
    // CA multicast only and must not leak this loopback channel onto
    // the wire.
    sock.set_multicast_loop_v4(true)?;
    sock.set_multicast_ttl_v4(1)?;
    sock.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)?;

    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Bind succeeds on an ephemeral port, local addr is wildcard.
    #[tokio::test]
    async fn bind_succeeds_and_reports_wildcard_local_addr() {
        let sock = bind_loopback_mcast(0).expect("bind");
        let local = sock.local_addr().expect("local_addr");
        match local {
            SocketAddr::V4(v4) => {
                assert!(v4.ip().is_unspecified(), "expected 0.0.0.0, got {v4}");
                assert!(v4.port() != 0, "ephemeral port must be assigned");
            }
            _ => panic!("expected V4 local addr, got {local}"),
        }
    }

    /// IP_MULTICAST_LOOP=1 means a send to 224.0.0.128:port from this
    /// socket comes back to it. Confirms the join + loop + TTL + IF
    /// chain is wired correctly end-to-end.
    #[tokio::test]
    async fn self_loopback_send_is_received() {
        let sock = bind_loopback_mcast(0).expect("bind");
        let port = sock.local_addr().unwrap().port();
        let dest = SocketAddr::V4(SocketAddrV4::new(ORIGIN_TAG_MCAST_GROUP, port));

        sock.send_to(b"origin-tag-loop", dest).await.expect("send");

        let mut buf = [0u8; 64];
        let (n, _src) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("recv timeout")
            .expect("recv ok");
        assert_eq!(&buf[..n], b"origin-tag-loop");
    }

    /// Two independent sockets co-bound to the same port (SO_REUSEPORT)
    /// both receive the same multicast — the cross-process forwarding
    /// scenario pvxs depends on. Skipped on Windows where the unix-
    /// only `set_reuse_port` isn't applied.
    #[cfg(unix)]
    #[tokio::test]
    async fn two_listeners_both_receive() {
        let a = bind_loopback_mcast(0).expect("bind a");
        let port = a.local_addr().unwrap().port();
        let b = bind_loopback_mcast(port).expect("bind b shares port");

        let dest = SocketAddr::V4(SocketAddrV4::new(ORIGIN_TAG_MCAST_GROUP, port));
        a.send_to(b"shared", dest).await.expect("send");

        let mut buf_a = [0u8; 32];
        let mut buf_b = [0u8; 32];
        let (na, _) = tokio::time::timeout(Duration::from_secs(2), a.recv_from(&mut buf_a))
            .await
            .expect("a recv timeout")
            .expect("a recv ok");
        let (nb, _) = tokio::time::timeout(Duration::from_secs(2), b.recv_from(&mut buf_b))
            .await
            .expect("b recv timeout")
            .expect("b recv ok");
        assert_eq!(&buf_a[..na], b"shared");
        assert_eq!(&buf_b[..nb], b"shared");
    }
}
