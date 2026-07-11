//! R7-16 end-to-end: a `HOST()` HAG rule must be matched against the
//! hostname the client claims over `CA_PROTO_HOST_NAME`.
//!
//! C rsrv's default (`asCheckClientIP == 0`, `asLibRoutines.c:34`) stores
//! the client-supplied name unconditionally (`camessage.c:845-875`) and
//! matches HAGs against it (`asLibRoutines.c:1223`). The port instead
//! keyed ACF on the peer IP unless `EPICS_CAS_USE_HOST_NAMES=YES` was set
//! — a variable that exists nowhere in epics-base — so an `.acf` that
//! granted WRITE under C granted nothing here.
//!
//! Pre-fix this test fails: the identity is `127.0.0.1`, the `HAG(ops)`
//! entry is `opi-01.lab`, no rule matches, and the put is denied.

use std::time::Duration;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let p = l.local_addr().expect("addr").port();
    drop(l);
    p
}

fn point_client_at(port: u16) {
    // SAFETY: the tests in this file are `#[serial]` and set the env before
    // `CaClient::new()` snapshots its resolver configuration.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// WRITE is granted only to the `ops` HAG, whose sole member is a host
/// *name* — the shape of every real facility `.acf`.
const ACF: &str = r#"
HAG(ops) { opi-01.lab }
ASG(DEFAULT) {
    RULE(1, READ)
    RULE(1, WRITE) { HAG(ops) }
}
"#;

async fn serve_with_acf(port: u16, acf_path: &std::path::Path) {
    let server = CaServer::builder()
        .port(port)
        .pv("SEC:VAL", EpicsValue::Long(0))
        .acf_file(acf_path.to_str().unwrap())
        .expect("load acf")
        .build()
        .await
        .expect("build CA server");
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// The client claims the hostname the HAG names → C grants WRITE, so the
/// put must succeed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn claimed_host_name_matches_a_host_hag_and_grants_write() {
    let dir = tempfile::tempdir().expect("temp");
    let acf_path = dir.path().join("host.acf");
    std::fs::write(&acf_path, ACF).expect("write acf");

    let port = free_port();
    serve_with_acf(port, &acf_path).await;
    point_client_at(port);

    let client = CaClient::new().await.expect("client");
    // The name libca sends in CA_PROTO_HOST_NAME. C's rsrv believes it;
    // so must we.
    client.set_host_name("opi-01.lab");

    let ch = client.create_channel("SEC:VAL");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");

    ch.put(&EpicsValue::Long(42))
        .await
        .expect("HAG(ops) { opi-01.lab } must grant WRITE to a client claiming that name");

    let (_ty, read) = ch.get().await.expect("get");
    assert_eq!(
        read,
        EpicsValue::Long(42),
        "the put must actually have landed"
    );
}

/// The other half of the same contract: a client claiming a name the HAG
/// does not list gets no WRITE. Without this, "grants write" above could
/// pass on a server that simply ignores the HAG.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn unlisted_host_name_is_denied_write() {
    let dir = tempfile::tempdir().expect("temp");
    let acf_path = dir.path().join("host.acf");
    std::fs::write(&acf_path, ACF).expect("write acf");

    let port = free_port();
    serve_with_acf(port, &acf_path).await;
    point_client_at(port);

    let client = CaClient::new().await.expect("client");
    client.set_host_name("intruder.example");

    let ch = client.create_channel("SEC:VAL");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");

    let put = ch.put(&EpicsValue::Long(7)).await;
    assert!(
        put.is_err(),
        "a host outside HAG(ops) must not get WRITE; got {put:?}"
    );
}
