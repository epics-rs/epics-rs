//! R18-62: `SO_REUSEPORT` is set on the fresh socket, before bind/connect.
//!
//! C `connectIt` creates the socket (drvAsynIPPort.c:442), sets `SO_BROADCAST`
//! (:451-459) and `SO_REUSEPORT` (:464-477) on it, and only then binds the local
//! address (:499-506) and connects (:513-523). The kernel honours `SO_REUSEPORT`
//! only on an unbound socket, so the order is the feature: `udp&` exists so two
//! ports can share one local port.
//!
//! Before the fix the driver bound first and set the option afterwards, so the
//! second `udp&` port on the same local port failed `EADDRINUSE` — the one
//! configuration the `&` suffix is for.

use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::port::PortDriver;
use asyn_rs::user::AsynUser;

/// Take a local UDP port by binding one and dropping it: the two `udp` ports
/// below both bind that number. Used only by the negative-control test, which
/// cannot hold the number (the first plain bind must own it) and instead retries
/// the whole scenario on a steal — see there.
fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

/// Hold a UDP port alive for the whole test so no neighbour's probe can steal
/// the number in the drop→rebind window (the probe-then-rebind flake).
///
/// The driver binds its `udp&` local endpoint on `0.0.0.0:<port>` with
/// `SO_REUSEPORT` set before the bind (`ip_port.rs::new_socket` /
/// `connect_udp`). Holding a `SO_REUSEPORT` socket on the *same* `0.0.0.0:<port>`
/// therefore (a) reserves the number — a plain-bind neighbour is refused
/// `EADDRINUSE`, and the ephemeral allocator will not hand it out — while
/// (b) still letting the driver's two `udp&` ports join the same REUSEPORT group.
/// Returns `(held socket, port)`; keep the socket alive for as long as the number
/// must stay ours.
fn hold_reuseport_udp() -> (Socket, u16) {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    // Same split as the driver's own bind (ip_port.rs `new_socket`, C
    // drvAsynIPPort.c:465-469 USE_SO_REUSEADDR): SO_REUSEPORT where it
    // exists, SO_REUSEADDR on Windows — socket2 exposes `set_reuse_port`
    // only on unix, and Windows SO_REUSEADDR is what grants the co-bind.
    #[cfg(unix)]
    s.set_reuse_port(true).unwrap();
    #[cfg(not(unix))]
    s.set_reuse_address(true).unwrap();
    let bind: SocketAddr = (Ipv4Addr::UNSPECIFIED, 0).into();
    s.bind(&bind.into()).unwrap();
    let port = s.local_addr().unwrap().as_socket().unwrap().port();
    (s, port)
}

/// ```text
/// drvAsynIPPortConfigure("A", "127.0.0.1:9000:5000 udp&", ...)
/// drvAsynIPPortConfigure("B", "127.0.0.1:9000:5000 udp&", ...)
/// ```
///
/// Both ports bind local port 5000. That is what `SO_REUSEPORT` buys, and it
/// only works if the option precedes the bind.
#[test]
fn two_udp_reuseport_ports_share_one_local_port() {
    // `local` is what both ports re-bind, so hold it race-proof with a REUSEPORT
    // socket the driver's own REUSEPORT binds can share. `peer` is only a
    // destination address (the driver never binds it, C :513 does not connect a
    // datagram socket), so a plain held socket keeps its number ours too. Holding
    // both across the whole test closes the probe-then-rebind window without
    // changing the honest path — the ports still bind `local`, now alongside our
    // held member of the same REUSEPORT group.
    let (_hold_local, local) = hold_reuseport_udp();
    let (_hold_peer, peer) = hold_reuseport_udp();
    let spec = format!("127.0.0.1:{peer}:{local} udp&");

    let mut first = DrvAsynIPPort::new("R1862A", &spec).unwrap();
    first
        .connect(&AsynUser::default())
        .expect("first udp& port must bind its local port");

    let mut second = DrvAsynIPPort::new("R1862B", &spec).unwrap();
    second.connect(&AsynUser::default()).expect(
        "a second udp& port must bind the SAME local port — SO_REUSEPORT has to be \
         set before the bind (C drvAsynIPPort.c:464-477 then :499-506). Setting it \
         on the already-bound socket leaves this EADDRINUSE.",
    );

    assert!(first.base().is_connected());
    assert!(second.base().is_connected());
}

/// The negative control: plain `udp` sets no `SO_REUSEPORT`, so the second port
/// on the same local port must still be refused (C only sets the option under
/// `FLAG_SO_REUSEPORT`).
#[test]
fn plain_udp_still_refuses_a_second_bind_of_one_local_port() {
    // This test cannot hold `local` — the whole point is that the FIRST plain
    // bind must own it exclusively, which a held socket would itself block. So
    // key the flake out with a bounded retry instead: a plain-`udp` first bind of
    // a truly-free port always succeeds, so a first-bind failure can only be a
    // neighbour stealing the number in the probe→bind window; retry with a fresh
    // number. The SECOND bind's `EADDRINUSE` is the property under test — that is
    // asserted, never retried.
    for _ in 0..50 {
        let peer = free_udp_port();
        let local = free_udp_port();
        let spec = format!("127.0.0.1:{peer}:{local} udp");

        let mut first = DrvAsynIPPort::new("R1862C", &spec).unwrap();
        match first.connect(&AsynUser::default()) {
            Ok(()) => {}
            // The driver wraps the bind EADDRINUSE as "UDP bind '...' failed"
            // (ip_port.rs::connect_udp). On the first bind that can only be a
            // steal of the just-probed number — try a fresh one.
            Err(e) if e.message().contains("UDP bind") => continue,
            Err(e) => panic!(
                "first udp bind failed for a non-steal reason: {:?}",
                e.message()
            ),
        }

        let mut second = DrvAsynIPPort::new("R1862D", &spec).unwrap();
        let err = second
            .connect(&AsynUser::default())
            .expect_err("without the & suffix the local port is exclusive");
        assert!(
            err.message().contains("UDP bind"),
            "the refusal must come from the bind, got {:?}",
            err.message()
        );
        return;
    }
    panic!("a neighbour kept stealing the probed local port across 50 attempts");
}
