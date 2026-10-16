//! A softIocPVX whose CA port is stolen is rerun on a fresh one. RSRV's
//! UDP bind fails silently on EADDRINUSE and `cantProceed` suspends iocInit
//! (caservertask.c:131-145, :751-752), so without the rerun the spawn waits
//! out its budget on an IOC that will never serve.

// No exec-model census marker: the test is a plain `#[test]`.

use super::interop_helpers::{
    PvxsLogs, SOFT_IOC_PVX, ioc_ready, pick_localhost_port, port_collision, require_pvxs,
    soft_ioc_pvx_command, start_pvxs,
};

#[test]
fn a_held_ca_port_is_rerun_on_a_fresh_one() {
    let ioc_bin = require_pvxs(SOFT_IOC_PVX);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("t.db");
    std::fs::write(&db, "record(ai, \"CAP:AI\") {}\n").expect("write db");
    let held = named_port::free_for_tcp_and_udp();
    let _thief = std::net::UdpSocket::bind(("127.0.0.1", held)).expect("hold the CA port");

    let logs = PvxsLogs::in_dir(dir.path(), "ioc");
    let mut ca_ports = Vec::new();
    let build = || {
        let ca = if ca_ports.is_empty() {
            held
        } else {
            named_port::free_for_tcp_and_udp()
        };
        ca_ports.push(ca);
        let tcp = pick_localhost_port();
        (
            soft_ioc_pvx_command(&ioc_bin, &db, tcp, ca),
            std::net::SocketAddr::from(([127, 0, 0, 1], tcp)),
        )
    };
    let started = start_pvxs(&logs, build, ioc_ready);
    assert!(started.is_ok(), "{}", started.err().unwrap_or_default());
    // A fresh port is a probe released before the IOC binds it, so a parallel
    // test can take it too and cost another rerun: the count is not fixed.
    // What is fixed is that the first attempt ran on the held UDP port and
    // lost it, and that every later rerun was one `start_pvxs` itself
    // reruns on (`port_collision`): a UDP, TCP or PVA steal, not a whim.
    assert!(ca_ports.len() >= 2, "no rerun: {ca_ports:?}");
    assert_eq!(ca_ports[0], held, "{ca_ports:?}");
    let first = logs.kept(1).text();
    assert!(
        first.contains(named_port::verdict::NO_TCP_SERVER),
        "{first}"
    );
    for n in 2..ca_ports.len() {
        let lost = logs.kept(n).text();
        assert!(
            port_collision(&lost),
            "attempt {n} on CA port {} was rerun without losing a port: {lost}",
            ca_ports[n - 1]
        );
    }
}
