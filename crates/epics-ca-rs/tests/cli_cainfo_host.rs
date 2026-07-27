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

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]
// …and `client`, because the reverse-resolved name IS the `hostname` module
// (`lib.rs`, `#[cfg(feature = "client")]`). `client-core` naming a peer by its
// dotted address is that feature's stated behaviour, not a regression, so this
// file asserts something only the full client claims.
#![cfg(feature = "client")]

use std::process::Command;

use epics_base_rs::server::records::longout::LongoutRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cainfo_prints_the_resolved_host_name() {
    // The server TAKES its port by binding it (`.port(0)` → read back
    // `udp_port()`); nothing probes a port and hands the number on.
    let server = CaServer::builder()
        .port(0)
        .record("TST:INFO", LongoutRecord::new(7))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    // `Host:` names the CIRCUIT peer, so it carries the TCP port — a separate
    // ephemeral from the search port when the server binds `.port(0)`.
    let tcp_port = server.tcp_port();
    tokio::spawn(async move { server.run().await });

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
    assert_eq!(p, tcp_port.to_string(), "{stdout:?}");
    // …and what precedes it is the name the resolver gave for 127.0.0.1, which
    // is the whole finding. Not pinned to the literal `localhost`: the PTR for
    // the loopback comes from the machine's own `/etc/hosts`.
    assert_ne!(
        name, "127.0.0.1",
        "C prints ca_host_name(chid), a reverse-resolved name (cainfo.c:101): {stdout:?}"
    );
    assert!(!name.is_empty(), "{stdout:?}");
}
