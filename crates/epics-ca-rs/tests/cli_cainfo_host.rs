//! Regression test (W10-B5): `cainfo`'s `Host:` line is the reverse-resolved
//! host name, not the dotted IP.
//!
//! C prints `ca_host_name(chid)` (`cainfo.c:101`), which reads the circuit's
//! `hostNameCache` — libcom's `ipAddrToA` (`osiSock.c:92-114`): `getnameinfo`
//! with `NI_NAMEREQD`, then `:<port>` appended, and the dotted IP only when the
//! address has no PTR record. Head-to-head against a softIoc on the loopback:
//!
//! ```text
//! C : Host:             localhost:5064
//! RS: Host:             127.0.0.1:5064        (pre-fix)
//! ```
//!
//! The port was printing the raw peer `SocketAddr`, so every `cainfo` on a
//! resolvable IOC differed from C on the field operators grep most.

use std::process::Command;
use std::time::Duration;

use epics_base_rs::server::records::longout::LongoutRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
    let p = probe.local_addr().unwrap().port();
    drop(probe);
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cainfo_prints_the_resolved_host_name() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .record("TST:INFO", LongoutRecord::new(7))
        .build()
        .await
        .expect("build CA server");
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_cainfo-rs"))
            .arg("TST:INFO")
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .output()
            .expect("spawn cainfo-rs")
    })
    .await
    .expect("cainfo-rs child joined");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let host = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Host:"))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no `Host:` line: {stdout:?}"));

    // The port is still there — `ipAddrToA` appends `:%hu` to the NAME.
    let (name, p) = host.rsplit_once(':').expect("`<host>:<port>`");
    assert_eq!(p, port.to_string(), "{stdout:?}");
    // …and what precedes it is the name the resolver gave for 127.0.0.1, which
    // is the whole finding. Not pinned to the literal `localhost`: the PTR for
    // the loopback comes from the machine's own `/etc/hosts`.
    assert_ne!(
        name, "127.0.0.1",
        "C prints ca_host_name(chid), a reverse-resolved name (cainfo.c:101): {stdout:?}"
    );
    assert!(!name.is_empty(), "{stdout:?}");
}
