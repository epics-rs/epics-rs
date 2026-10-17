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

use super::interop_helpers::PvxsOutput;
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
    let pvxlist = require_pvxs("pvxlist");

    // pvxlist must bind the beacon port itself, so it is picked by binding
    // and releasing; another bind can take it in between, and pvxlist then
    // exits with `Address already in use`. Such a run is retried on a fresh
    // port, as the oracle's pvatool BEACON_ATTEMPTS does.
    const BEACON_ATTEMPTS: usize = 8;
    let mut attempts = 0;
    let (out, tcp_addr) = loop {
        attempts += 1;
        let (out, tcp_addr) = beacon_a_attempt(&pvxlist).await;
        if attempts == BEACON_ATTEMPTS
            || !String::from_utf8_lossy(&out.stderr).contains("Address already in use")
        {
            break (out, tcp_addr);
        }
    };

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

/// One Direction-A run: a Rust server beaconing to a fresh port and a
/// pvxlist listening on it. Returns pvxlist's output and the server's address.
#[cfg(tokio_backend)]
async fn beacon_a_attempt(
    pvxlist: &std::path::Path,
) -> (std::process::Output, std::net::SocketAddr) {
    // Pick an ephemeral UDP port. We tell BOTH pvxlist (via env)
    // and the Rust server (via beacon_destinations) to use it. Held
    // until pvxlist starts, so neither the server's search port nor
    // any socket the server binds can take it.
    let beacon_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let beacon_port = beacon_sock.local_addr().unwrap().port();

    let source = SharedSource::new();
    // Empty source — beacon emission is independent of hosted PVs.
    let cfg = PvaServerConfig {
        tcp_port: 0,
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

    let pvxlist_p = pvxlist.to_path_buf();
    let lib = pvxs_lib_dir().into_os_string();
    drop(beacon_sock);
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
            .pvxs_output()
            .expect("pvxlist exec")
    })
    .await
    .expect("join pvxlist");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    (out, tcp_addr)
}

/// Direction B: pvxs softIocPVX emits beacons; Rust client's
/// `discover()` API receives at least one `Discovered` event for
/// the pvxs server's GUID + address. Pre-batch-10 the SearchEngine
/// beacon-listen path was Rust↔Rust only; this confirms a real
/// pvxs beacon datagram parses cleanly through the Rust
/// `udp_collector` + onBeacon dispatch.
///
/// The beacon is sent to a non-loopback NIC address, as a beacon
/// from another host arrives. Fails when the host has no suitable
/// non-loopback IPv4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_beacon_b_rust_client_receives_pvxs_beacons() {
    let softioc = super::interop_helpers::require_pvxs(super::interop_helpers::SOFT_IOC_PVX);
    let dbd_path = super::interop_helpers::pvxs_dbd_dir().join("softIocPVX.dbd");
    assert!(dbd_path.is_file(), "softIocPVX.dbd missing: {dbd_path:?}");

    // Find a non-loopback IPv4 to send the beacon to.
    let local_v4 = local_non_loopback_v4()
        .expect("no non-loopback IPv4 on this host; the pvxs beacon test needs one");

    // The agreed beacon / search broadcast port between pvxs and the Rust
    // client, which binds it on the wildcard: owned there and opened to
    // that fanout co-bind, so nobody else can take it before the client has.
    let (beacon_holder, beacon_port) = {
        use epics_base_rs::net::{PortOrigin, PortUse};
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let any = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0));
        PortUse::Shared.bind(&s, any, PortOrigin::Chosen).unwrap();
        let p = s.local_addr().unwrap().as_socket().unwrap().port();
        (s, p)
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("beacon.db");
    std::fs::write(
        &db_path,
        "record(stringout, \"B:HOST\") { field(VAL, \"ok\") }\n",
    )
    .expect("write db");
    let logs = super::interop_helpers::PvxsLogs::in_dir(dir.path(), "ioc");
    let build = || {
        // The server's own search port: on the beacon port its wildcard UDP
        // collector would share a REUSEPORT group with the client's and take a
        // unicast beacon by hash.
        let search_port: u16 = {
            let s = std::net::UdpSocket::bind((local_v4, 0)).unwrap();
            s.local_addr().unwrap().port()
        };
        let pva_port: u16 = {
            let l = std::net::TcpListener::bind((local_v4, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let ca = named_port::free_for_tcp_and_udp();
        let mut cmd = pvxs_command(&softioc);
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
            .env("EPICS_PVAS_BROADCAST_PORT", search_port.to_string())
            // A named CA port: `0` falls back to 5064 (`envSubr.c:410`),
            // and a lost one is rerun through `port_collision`.
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_SERVER_PORT", ca.to_string())
            .env("EPICS_CA_SERVER_PORT", ca.to_string());
        (cmd, std::net::SocketAddr::from((local_v4, pva_port)))
    };

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
    // Listening before softIocPVX starts: pvxs sends one beacon at start
    // and the next 15 s later (server.cpp beaconIntervalShort).
    let mut rx = client.discover().await.expect("client.discover()");
    // The client holds the number now; the holder would take a share of the
    // beacons.
    drop(beacon_holder);

    let (ioc, tcp_addr) =
        match super::interop_helpers::start_pvxs(&logs, build, super::interop_helpers::ioc_ready) {
            Ok(started) => started,
            Err(text) => panic!(
                "softIocPVX did not come up. A spawned IOC that never comes up \
                 is a broken reference, not a missing one.\n{text}"
            ),
        };
    let pva_port = tcp_addr.port();

    // Up to 4 s for pvxs's beacon, which carries its TCP server endpoint. An
    // attempt `start_pvxs` reran beaconed before it was killed, on its own
    // port, so other servers' events are passed over.
    let mut seen = Vec::new();
    let discovered = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(event) = rx.recv().await {
            let text = format!("{event:?}");
            if text.contains(&pva_port.to_string()) {
                return true;
            }
            seen.push(text);
        }
        false
    })
    .await;

    drop(ioc);

    assert_eq!(
        discovered,
        Ok(true),
        "no beacon carried pvxs's pva_port={pva_port}; saw {seen:?}",
    );
}

/// Return the first non-loopback IPv4 address bound on a UP
/// interface, or None if there isn't one.
fn local_non_loopback_v4() -> Option<std::net::Ipv4Addr> {
    let interfaces = epics_base_rs::net::iface_map::IfaceMap::new()
        .expect("getifaddrs must succeed on the host");
    interfaces
        .up_non_loopback()
        .into_iter()
        .map(|i| i.ip)
        .find(|v4| !v4.is_unspecified())
}
