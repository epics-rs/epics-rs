//! Regression tests (R9-16): the CA tools gate their whole data phase on
//! the `connect_pvs` barrier.
//!
//! C `tool_lib.c::connect_pvs` (`:623-641`) creates every channel and then
//! waits for ALL of them in ONE `ca_pend_io(caTimeout)`. On `ECA_TIMEOUT`
//! it prints `Channel connect timed out: ...` and returns 1; `caget.c:553`,
//! `cainfo.c:228` and `caput.c:406` all gate on that return, so the
//! get/print/put phase never runs. With one missing PV among several, C
//! emits ZERO stdout value lines and exits 1.
//!
//! Pre-fix `caget-rs`/`cainfo-rs` connected each PV independently and
//! printed as results arrived, so a connected PV's value (or a
//! `State: connected` block) landed on stdout next to the missing PV's
//! marker; `caput-rs` printed its own `error: Timeout` instead of C's
//! diagnostic. These tests drive the real binaries against a live
//! `CaServer`, so they assert the observable contract: stdout, stderr and
//! the exit code.

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::server::CaServer;
use tokio::process::Command;

/// Bring up a server holding one scalar `ai` PV.
///
/// The server TAKES its port by binding it (`.port(0)` → read back
/// `udp_port()`); nothing probes a port and hands the number on.
async fn server_with_ai(pv: &'static str, val: f64) -> u16 {
    let server = CaServer::builder()
        .port(0)
        .record(pv, AiRecord::new(val))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

/// Run one CA tool binary against the test server. The child gets its CA
/// addressing purely from the environment, so nothing in this process is
/// mutated.
async fn run_tool(bin: &str, port: u16, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .output()
        .await
        .expect("run CA tool")
}

const CAGET: &str = env!("CARGO_BIN_EXE_caget-rs");
const CAINFO: &str = env!("CARGO_BIN_EXE_cainfo-rs");
const CAPUT: &str = env!("CARGO_BIN_EXE_caput-rs");

/// One missing PV among several: C prints no value line for the PV that
/// DID connect, and exits 1 (`caget.c:553-556`).
#[tokio::test(flavor = "multi_thread")]
async fn caget_prints_nothing_when_one_pv_of_many_never_connects() {
    let port = server_with_ai("R916:GET:A", 3.5).await;
    let out = run_tool(
        CAGET,
        port,
        &["-w", "0.5", "R916:GET:A", "R916:GET:MISSING"],
    )
    .await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout, "",
        "the connect barrier failed, so NO value line may be printed; got: {stdout:?}"
    );
    assert!(
        stderr.contains("Channel connect timed out: some PV(s) not found."),
        "expected C's multi-PV connect diagnostic, got: {stderr:?}"
    );
    assert_eq!(out.status.code(), Some(1), "C returns 1 from connect_pvs");
}

/// Single missing PV: C names it (`tool_lib.c:633-635`).
#[tokio::test(flavor = "multi_thread")]
async fn caget_names_the_single_missing_pv() {
    let port = server_with_ai("R916:GET:ONLY", 1.0).await;
    let out = run_tool(CAGET, port, &["-w", "0.5", "R916:GET:NOPE"]).await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout, "", "no stdout on a failed connect barrier");
    assert!(
        stderr.contains("Channel connect timed out: 'R916:GET:NOPE' not found."),
        "expected C's single-PV connect diagnostic, got: {stderr:?}"
    );
    assert_eq!(out.status.code(), Some(1));
}

/// The barrier must not break the all-connected path: every value still
/// prints, exit 0.
#[tokio::test(flavor = "multi_thread")]
async fn caget_still_prints_every_value_when_all_connect() {
    let port = server_with_ai("R916:GET:OK", 7.25).await;
    let out = run_tool(CAGET, port, &["-w", "2", "R916:GET:OK"]).await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("R916:GET:OK") && stdout.contains("7.25"),
        "connected PV must still print its value, got: {stdout:?}"
    );
    assert_eq!(out.status.code(), Some(0));
}

/// `cainfo` gates the same way (`cainfo.c:228-232`): a missing PV means the
/// per-PV block never prints — not even a `State: never connected` stanza,
/// and not the connected PV's block.
#[tokio::test(flavor = "multi_thread")]
async fn cainfo_prints_no_block_when_one_pv_never_connects() {
    let port = server_with_ai("R916:INFO:A", 2.0).await;
    let out = run_tool(
        CAINFO,
        port,
        &["-w", "0.5", "R916:INFO:A", "R916:INFO:MISSING"],
    )
    .await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout, "",
        "cainfo prints nothing when the connect barrier fails; got: {stdout:?}"
    );
    assert!(
        stderr.contains("Channel connect timed out: some PV(s) not found."),
        "expected C's connect diagnostic, got: {stderr:?}"
    );
    assert_eq!(out.status.code(), Some(1));
}

/// `caput` runs the same barrier (`caput.c:406-410`) and therefore prints
/// C's connect diagnostic — not a port-specific `error: ...` line.
#[tokio::test(flavor = "multi_thread")]
async fn caput_prints_the_c_connect_diagnostic() {
    let port = server_with_ai("R916:PUT:A", 0.0).await;
    let out = run_tool(CAPUT, port, &["-w", "0.5", "R916:PUT:MISSING", "1"]).await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout, "",
        "no put, no echo, when the channel never connects"
    );
    assert!(
        stderr.contains("Channel connect timed out: 'R916:PUT:MISSING' not found."),
        "expected C's single-PV connect diagnostic, got: {stderr:?}"
    );
    assert_eq!(out.status.code(), Some(1));
}
