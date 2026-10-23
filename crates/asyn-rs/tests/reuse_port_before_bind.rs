//! R18-62: `SO_REUSEPORT` is set on the fresh socket, before bind/connect.
//!
//! C `connectIt` creates the socket (drvAsynIPPort.c:443), sets `SO_BROADCAST`
//! (:453-462) and `SO_REUSEPORT` (:467-481) on it, and only then binds the local
//! address (:499-506) and connects (:533-577). The kernel honours `SO_REUSEPORT`
//! only on an unbound socket, so the order is the feature: `udp&` exists so two
//! ports can share one local port.
//!
//! Before the fix the driver bound first and set the option afterwards, so the
//! second `udp&` port on the same local port failed `EADDRINUSE` — the one
//! configuration the `&` suffix is for.

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

/// ```text
/// drvAsynIPPortConfigure("A", "127.0.0.1:9000:5000 udp&", ...)
/// drvAsynIPPortConfigure("B", "127.0.0.1:9000:5000 udp&", ...)
/// ```
///
/// Both ports bind local port 5000. That is what `SO_REUSEPORT` buys, and it
/// only works if the option precedes the bind.
#[test]
fn two_udp_reuseport_ports_share_one_local_port() {
    // `local` is what both ports re-bind, so it is held by a member of the
    // REUSEPORT group the driver's binds join. `peer` is only a destination
    // address (the driver never binds it, C :533 does not connect a datagram
    // socket), so a closed hold keeps its number ours.
    let hold_local = named_port::HeldPort::reuse_port_udp();
    let hold_peer = named_port::HeldPort::closed_udp();
    let (local, peer) = (hold_local.port, hold_peer.port);
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
            // A bind EADDRINUSE is C's "unable to bind to local port"
            // (drvAsynIPPort.c:500-505). On the first bind that can only be a
            // steal of the just-probed number — try a fresh one.
            Err(e) if e.message().starts_with("unable to bind to local port: ") => continue,
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
            err.message().starts_with("unable to bind to local port: "),
            "the refusal must come from the bind, got {:?}",
            err.message()
        );
        return;
    }
    panic!("a neighbour kept stealing the probed local port across 50 attempts");
}
