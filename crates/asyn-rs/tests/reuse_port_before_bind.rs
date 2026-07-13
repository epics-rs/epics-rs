//! R18-62: `SO_REUSEPORT` is set on the fresh socket, before bind/connect.
//!
//! C `connectIt` creates the socket (drvAsynIPPort.c:442), sets `SO_BROADCAST`
//! (:448-459) and `SO_REUSEPORT` (:461-477) on it, and only then binds the local
//! address (:495-506) and connects (:508-540). The kernel honours `SO_REUSEPORT`
//! only on an unbound socket, so the order is the feature: `udp&` exists so two
//! ports can share one local port.
//!
//! Before the fix the driver bound first and set the option afterwards, so the
//! second `udp&` port on the same local port failed `EADDRINUSE` — the one
//! configuration the `&` suffix is for.

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::port::PortDriver;
use asyn_rs::user::AsynUser;

/// Take a local UDP port by binding one and dropping it: the two `udp&` ports
/// below both bind that number.
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
    let peer = free_udp_port();
    let local = free_udp_port();
    let spec = format!("127.0.0.1:{peer}:{local} udp&");

    let mut first = DrvAsynIPPort::new("R1862A", &spec).unwrap();
    first
        .connect(&AsynUser::default())
        .expect("first udp& port must bind its local port");

    let mut second = DrvAsynIPPort::new("R1862B", &spec).unwrap();
    second.connect(&AsynUser::default()).expect(
        "a second udp& port must bind the SAME local port — SO_REUSEPORT has to be \
         set before the bind (C drvAsynIPPort.c:461-477 then :495-506). Setting it \
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
    let peer = free_udp_port();
    let local = free_udp_port();
    let spec = format!("127.0.0.1:{peer}:{local} udp");

    let mut first = DrvAsynIPPort::new("R1862C", &spec).unwrap();
    first.connect(&AsynUser::default()).expect("first udp bind");

    let mut second = DrvAsynIPPort::new("R1862D", &spec).unwrap();
    let err = second
        .connect(&AsynUser::default())
        .expect_err("without the & suffix the local port is exclusive");
    assert!(
        err.message().contains("UDP bind"),
        "the refusal must come from the bind, got {:?}",
        err.message()
    );
}
