//! R6-30 — a TCP connect/disconnect must not restart the beacon ramp.
//!
//! C `rsrv_online_notify_task` sets the ramp's initial period exactly once, at
//! task start (`online_notify.c:68` `delay = 0.02`), and restarts it in exactly
//! one other place: the `beacon_ctl == ctlPause` wait loop
//! (`online_notify.c:126-129`). Accepting or losing a client connection never
//! touches it. The port pulsed `beacon_reset` on every accept and every
//! disconnect — self-described as a "Rust enhancement" — which restarts the
//! 20 ms ramp and, with R6-23's libca anomaly bands installed, makes every
//! other client of that server flag ShortPeriod beacon anomalies.

// Host/tokio-only: builds the async `CaClient`/`CaServer` stack in process.
// Under `rtems-exec-model` the `runtime::task` seam routes their `spawn`
// to the background executor, whose worker has no tokio reactor, so the
// listener/transport tasks panic. The RTEMS model serves from
// `BlockingCaServer` instead, so this path is inapplicable there.
#![cfg(not(feature = "rtems-exec-model"))]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
use tokio::net::UdpSocket;

/// Drive TCP connect/disconnect churn against the server while counting the
/// beacons it emits. The ramp doubles 20 ms → 40 → 80 … up to the configured
/// max period, so after ~1 s of warm-up the next beacon is at least ~0.5 s
/// away and a 1 s observation window can hold only a couple of beacons. Every
/// connect/disconnect that restarts the ramp, in contrast, emits a beacon
/// immediately (the emitter's `reset.notified()` arm re-enters the send loop),
/// so the pre-fix server answers this churn with a beacon per socket event.
#[tokio::test]
async fn r6_30_tcp_connect_disconnect_does_not_restart_the_beacon_ramp() {
    let sink = UdpSocket::bind("127.0.0.1:0").await.expect("beacon sink");
    let sink_port = sink.local_addr().unwrap().port();

    // SAFETY: nextest runs each test in its own process, so no other test
    // mutates the environment concurrently; both vars are read when the
    // server's UDP/beacon config is built inside `run()` below.
    unsafe {
        std::env::set_var(
            "EPICS_CAS_BEACON_ADDR_LIST",
            format!("127.0.0.1:{sink_port}"),
        );
        // Max ramp period. Large enough that the ramp is still doubling
        // (never clamped) for the whole test.
        std::env::set_var("EPICS_CA_BEACON_PERIOD", "10");
    }

    // The server TAKES its port by binding it (`.port(0)` → read back the
    // bound TCP port); nothing probes a port and hands the number on. The
    // churn below connects to the TCP listener, which under `.port(0)` is a
    // different ephemeral from the UDP search port.
    let server = CaServer::builder()
        .port(0)
        .pv("R6:30:PV", EpicsValue::Long(0))
        .build()
        .await
        .expect("build CA server");
    let server_port = server.tcp_port();
    let _h = tokio::spawn(async move { server.run().await });

    // Warm-up: let the ramp climb out of its fast phase (20+40+80+160+320 ms
    // ≈ 0.6 s of beacons; by ~1.2 s the next period is ≥ 640 ms).
    let mut buf = [0u8; 64];
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_millis(1200) {
        let _ = tokio::time::timeout(Duration::from_millis(50), sink.recv(&mut buf)).await;
    }

    // Observation window: 5 connect/disconnect cycles, one every 100 ms.
    // Pre-fix that is 10 ramp restarts → ≥ 10 beacons; post-fix the ramp is
    // untouched and keeps doubling from ≥ 640 ms.
    let churn = tokio::task::spawn_blocking(move || {
        for _ in 0..5 {
            let sock = TcpStream::connect(("127.0.0.1", server_port)).expect("connect");
            std::thread::sleep(Duration::from_millis(50));
            drop(sock); // disconnect
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let mut beacons = 0usize;
    let window = Instant::now();
    while window.elapsed() < Duration::from_millis(1000) {
        if tokio::time::timeout(Duration::from_millis(25), sink.recv(&mut buf))
            .await
            .is_ok()
        {
            beacons += 1;
        }
    }
    churn.await.expect("churn task");

    assert!(
        beacons <= 4,
        "the beacon ramp must be blind to TCP connect/disconnect (C online_notify.c \
         restarts it only at task start and on ctlPause); saw {beacons} beacons during \
         5 connect+disconnect cycles, which means each socket event is still pulsing \
         beacon_reset"
    );
}
