//! A softIocPVX whose CA port is stolen is rerun on a fresh one. RSRV's
//! UDP bind fails silently on EADDRINUSE and `cantProceed` suspends iocInit
//! (caservertask.c:131-145, :751-752), so without the rerun the spawn waits
//! out its budget on an IOC that will never serve.

// No exec-model census marker: the test is a plain `#[test]`.

use super::interop_helpers::{
    PvxsLogs, SOFT_IOC_PVX, ioc_ready, pick_localhost_port, require_pvxs, soft_ioc_pvx_command,
    start_pvxs,
};

#[test]
fn a_held_ca_port_is_rerun_on_a_fresh_one() {
    let Some(ioc_bin) = require_pvxs(SOFT_IOC_PVX) else {
        return;
    };
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
    assert_eq!(ca_ports.len(), 2, "{ca_ports:?}");
    let lost = logs.kept(1).text();
    assert!(lost.contains("CAS: No TCP server started"), "{lost}");
}
