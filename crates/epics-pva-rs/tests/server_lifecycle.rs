//! A server dies with the task that runs it. `PvaServer::wait` moves the
//! listener handles into its own future, so aborting a `run` task must
//! still release the TCP listener and the UDP responder.

#![cfg(tokio_backend)]

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::server::PvaServer;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

// Probe the wildcard the listener binds: Windows lets a specific address
// bind beside a wildcard listener.
fn tcp_is_held(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_err()
}

// Probe the loopback address the bundle binds. A wildcard bind does not
// see a bundle of more than ten per-NIC sockets (the kernel's secondary
// UDP hash only compares the wildcard and the probe's own address), and the
// one wildcard socket the server owns is bound by its task, after the report.
fn udp_is_held(port: u16) -> bool {
    std::net::UdpSocket::bind(("127.0.0.1", port)).is_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_run_task_releases_both_ports() {
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_SERVER_PORT", "0");
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", "0");
    }
    let db = Arc::new(PvDatabase::new());
    db.add_pv("LIFE:X", EpicsValue::Double(1.0)).await.unwrap();
    let server = PvaServer::from_parts(
        db,
        None,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    );
    let (tx, mut rx) = watch::channel(None);
    let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
    let report = loop {
        if let Some(h) = rx.borrow_and_update().clone() {
            break h.report();
        }
        rx.changed().await.expect("server reports its ports");
    };
    let (tcp, udp) = (report.tcp_port, report.udp_port);
    assert!(tcp_is_held(tcp), "listener not bound");
    assert!(udp_is_held(udp), "responder not bound");

    run.abort();
    let _ = run.await;
    // The listener tasks are cancelled at their next poll, not inside
    // `abort`; give them a bounded time to drop their sockets.
    let deadline = Instant::now() + Duration::from_secs(5);
    while tcp_is_held(tcp) || udp_is_held(udp) {
        assert!(
            Instant::now() < deadline,
            "ports {tcp}/{udp} still bound after the run task was aborted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
