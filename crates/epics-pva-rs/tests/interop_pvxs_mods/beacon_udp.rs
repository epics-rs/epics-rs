//! Beacon UDP cross-impl. PVA servers emit periodic UDP beacons
//! so clients can discover them without active SEARCH. pvxlist
//! `-w N` listens on `EPICS_PVA_BROADCAST_PORT` and prints every
//! server it sees from beacons + SEARCH responses.
//!
//! - **Direction A**: Rust server with explicit `beacon_destinations`
//!   pointed at the broadcast port we tell pvxlist to use. After
//!   pvxlist's listen window, its stdout must contain the Rust
//!   server's `<ip>:<port>` advertisement.

use super::interop_helpers::{pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedSource};

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

fn env_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_beacon_a_pvxlist_discovers_rust_server() {
    let Some(pvxlist) = require_pvxs("pvxlist") else {
        return;
    };

    // Pick an ephemeral UDP port. We tell BOTH pvxlist (via env)
    // and the Rust server (via beacon_destinations) to use it.
    let beacon_port: u16 = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let p = s.local_addr().unwrap().port();
        drop(s);
        p
    };

    let source = SharedSource::new();
    // Empty source — beacon emission is independent of hosted PVs.
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let p = s.local_addr().unwrap().port();
            drop(s);
            p
        },
        beacon_destinations: vec![format!("127.0.0.1:{beacon_port}").parse().unwrap()],
        // Burst cadence: send 10 beacons at the short interval, so
        // pvxlist's 2 s listen window catches at least one even
        // when the test process is briefly preempted.
        beacon_period: Duration::from_millis(200),
        beacon_burst_count: 10,
        auto_beacon: false,
        ..PvaServerConfig::isolated()
    };
    let server = PvaServer::start(Arc::new(source), cfg).expect("server start");
    let tcp_addr = server.tcp_addr();

    // Give the beacon emitter a few ticks before pvxlist starts
    // listening; pvxs's pvxlist samples its listener at ~250 ms
    // intervals.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pvxlist_p = pvxlist.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxlist_p)
            .arg("-w")
            .arg("2") // 2-second listen window
            .env("EPICS_PVA_BROADCAST_PORT", beacon_port.to_string())
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            // Need an EMPTY addr list so pvxlist doesn't also try
            // active SEARCH (we want to test the beacon path
            // specifically).
            .env("EPICS_PVA_ADDR_LIST", "")
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxlist exec")
    })
    .await
    .expect("join pvxlist");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // pvxlist prints `<ip>:<tcp_port>` lines for each discovered
    // server. Our Rust server bound an ephemeral port; assert that
    // port appears in pvxlist's output (the IP is loopback —
    // either 127.0.0.1 or the host's primary IP depending on
    // beacon source-address selection).
    let want = format!(":{}", tcp_addr.port());
    assert!(
        stdout.contains(&want),
        "pvxlist did not discover Rust server via beacon (expected `{want}` substring).\n\
         pvxlist stdout: {stdout}\nstderr: {stderr}",
    );
}
