//! Beacon UDP cross-impl. PVA servers emit periodic UDP beacons
//! so clients can discover them without active SEARCH. pvxlist
//! `-w N` listens on `EPICS_PVA_BROADCAST_PORT` and prints every
//! server it sees from beacons + SEARCH responses.
//!
//! - **Direction A**: Rust server with explicit `beacon_destinations`
//!   pointed at the broadcast port we tell pvxlist to use. After
//!   pvxlist's listen window, its stdout must contain the Rust
//!   server's `<ip>:<port>` advertisement.
// The tests that drive a live server are `tokio_backend`-only, so on
// `exec_backend` the fixtures and imports they share go unreferenced while the
// rest of this file still runs. The default build lints it in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{pvxs_command, pvxs_lib_dir, require_pvxs};

#[cfg(tokio_backend)]
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{PvaServerConfig, SharedSource};

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

#[cfg(tokio_backend)]
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
        beacon_destinations: vec![
            format!("127.0.0.1:{beacon_port}")
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        ],
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

/// Direction B: pvxs softIocPVX emits beacons; Rust client's
/// `discover()` API receives at least one `Discovered` event for
/// the pvxs server's GUID + address. Pre-batch-10 the SearchEngine
/// beacon-listen path was Rust↔Rust only; this confirms a real
/// pvxs beacon datagram parses cleanly through the Rust
/// `udp_collector` + onBeacon dispatch.
///
/// The Rust client's `bind_beacon_udp` deliberately skips the
/// loopback NIC (to avoid SO_REUSEPORT collision with a co-hosted
/// Rust server), so this test cannot use 127.0.0.1 for beacons.
/// Instead pick a non-loopback NIC address from the host and use
/// that as both pvxs's beacon destination and the bind target the
/// Rust client picks up. SKIPs cleanly when the host has no
/// suitable non-loopback IPv4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_beacon_b_rust_client_receives_pvxs_beacons() {
    let Some(softioc) = super::interop_helpers::locate_pvxs(super::interop_helpers::SOFT_IOC_PVX)
    else {
        eprintln!("SKIP: softIocPVX not found");
        return;
    };
    let dbd_path = super::interop_helpers::pvxs_dbd_dir().join("softIocPVX.dbd");
    if !dbd_path.is_file() {
        eprintln!("SKIP: softIocPVX.dbd missing");
        return;
    }

    // Find a non-loopback IPv4 we can bind on (matches what
    // `bind_non_loopback` enumerates).
    let local_v4 = match local_non_loopback_v4() {
        Some(a) => a,
        None => {
            eprintln!("SKIP: no non-loopback IPv4 on this host");
            return;
        }
    };

    // Pick an ephemeral UDP port to act as the agreed beacon /
    // search broadcast port between pvxs and the Rust client.
    let beacon_port: u16 = {
        let s = std::net::UdpSocket::bind((local_v4, 0)).unwrap();
        let p = s.local_addr().unwrap().port();
        drop(s);
        p
    };
    let pva_port: u16 = {
        let l = std::net::TcpListener::bind((local_v4, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("beacon.db");
    std::fs::write(
        &db_path,
        "record(stringout, \"B:HOST\") { field(VAL, \"ok\") }\n",
    )
    .expect("write db");
    let stdout_path = dir.path().join("ioc.stdout");
    let stderr_path = dir.path().join("ioc.stderr");

    let mut cmd = super::interop_helpers::pvxs_command(&softioc);
    cmd.arg("-D")
        .arg(&dbd_path)
        .arg("-d")
        .arg(&db_path)
        .arg("-S")
        .env("EPICS_PVAS_SERVER_PORT", pva_port.to_string())
        .env("EPICS_PVAS_INTF_ADDR_LIST", local_v4.to_string())
        .env(
            "EPICS_PVAS_BEACON_ADDR_LIST",
            format!("{local_v4}:{beacon_port}"),
        )
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BROADCAST_PORT", beacon_port.to_string())
        // softIocPVX bundles a CA server too — keep it off the
        // default ports so we don't fight other IOCs on the host.
        .env("EPICS_CAS_SERVER_PORT", "0")
        .env("EPICS_CA_SERVER_PORT", "0")
        .stdout(std::process::Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ));

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => panic!("failed to spawn softIocPVX: {e}"),
    };
    // Wait for PVA TCP port to be live (proxies "IOC up").
    let tcp_addr: std::net::SocketAddr = format!("{local_v4}:{pva_port}").parse().unwrap();
    let mut up = false;
    for _ in 0..50 {
        if std::net::TcpStream::connect_timeout(&tcp_addr, Duration::from_millis(100)).is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !up {
        let _ = child.kill();
        let _ = child.wait();
        let out = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let err = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        panic!(
            "softIocPVX did not bind {tcp_addr} within 5s. A spawned IOC that \
             never comes up is a broken reference, not a missing one.\n\
             pvxs stdout:\n{out}\npvxs stderr:\n{err}"
        );
    }

    // Rust client listening on the same broadcast port, so pvxs's beacons
    // reach our UDP collector.
    //
    // This used to reach the client by mutating the process environment and
    // justify it with two claims, both false: that nextest serialises
    // within-binary tests (it runs each in its OWN process, which is a
    // different thing and says nothing about `cargo test`), and that the
    // test is single-threaded (it is `flavor = "multi_thread"`, under a
    // libtest harness that runs tests as threads). `set_var`'s contract is
    // that no other thread may touch the environment concurrently, and every
    // sibling test here reads it — `PvaClient`/`PvaServer` for their EPICS_*
    // config, `pvxs_command` for the library path. So a concurrent run was
    // both a lost setting and a data race on `environ`.
    //
    // The builder takes all three directly and documents each as replacing
    // what `new()` parsed from env, so nothing is left to share. It also pins
    // this client to its own search engine rather than the process-wide
    // shared one, which is the second thing a sibling could have taken from
    // it.
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .broadcast_port(beacon_port)
        .auto_addr_list(false)
        .addr_list(Vec::new())
        .build();
    let mut rx = match client.discover().await {
        Ok(r) => r,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("client.discover() failed: {e:?}");
        }
    };

    // Wait up to 4 s for the first Discovered event from pvxs.
    // softIocPVX bursts beacons every 200 ms by default for the
    // initial period — should see one well within 4 s.
    let discovered = tokio::time::timeout(Duration::from_secs(4), rx.recv()).await;

    let _ = child.kill();
    let _ = child.wait();

    let event = discovered
        .expect("timeout waiting for pvxs beacon")
        .expect("discover channel closed before any beacon");
    // pvxs's beacon carries its TCP server endpoint; assert the
    // discovered event matches the port we know pvxs bound.
    assert!(
        format!("{event:?}").contains(&pva_port.to_string()),
        "discovered event did not carry pvxs's pva_port={pva_port}: {event:?}",
    );
}

/// Return the first non-loopback IPv4 address bound on a UP
/// interface, or None if there isn't one. Matches what the Rust
/// client's `bind_non_loopback` UDP socket enumerates.
fn local_non_loopback_v4() -> Option<std::net::Ipv4Addr> {
    let interfaces = epics_base_rs::net::iface_map::IfaceMap::new()
        .expect("getifaddrs must succeed on the host");
    interfaces
        .up_non_loopback()
        .into_iter()
        .map(|i| i.ip)
        .find(|v4| !v4.is_unspecified())
}
