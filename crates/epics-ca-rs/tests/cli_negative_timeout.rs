//! Regression tests (W10-B1): a NEGATIVE `-w` is an already-expired deadline.
//!
//! C hands `caTimeout` straight to `ca_pend_io`, which turns it into
//! `now + caTimeout`. A negative value puts that deadline in the PAST, so the
//! wait returns `ECA_TIMEOUT` at once and `connect_pvs` (`tool_lib.c:628-638`)
//! reports the failure and exits 1 — even for a PV that is right there.
//! Observed on the compiled C (EPICS 7.0.10.1-DEV) against a live softIoc:
//!
//! ```text
//! caget -w -1 TST:AO   C: Channel connect timed out: 'TST:AO' not found.  exit 1
//!                     RS: TST:AO   1.5                                    exit 0   (pre-fix)
//! ```
//!
//! The port clamped every negative to the 1 s default — the opposite outcome.
//! `EPICS_CLI_TIMEOUT=-1` is the same timeout and behaves the same way in C.
//!
//! DEVIATION, deliberate: C's `-w nan` reaches `ca_pend_io(nan)`, where every
//! deadline comparison is false and the tool hangs forever. We keep treating
//! NaN (and +inf) as the default rather than reproducing that.

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

use std::process::Command;
use std::time::Duration;

use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::cli::{INDEFINITE_TIMEOUT, timeout_duration};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// (stdout, stderr, exit code) of the real `caget-rs`.
async fn caget(port: u16, args: &[&str]) -> (String, String, i32) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_caget-rs"))
            .args(&args)
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .output()
            .expect("spawn caget-rs")
    })
    .await
    .expect("caget-rs child joined");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// The server TAKES its port by binding it (`.port(0)` → read back
/// `udp_port()`); nothing probes a port and hands the number on.
async fn server_with_ao(pv: &'static str, val: f64) -> u16 {
    let server = CaServer::builder()
        .port(0)
        .record(pv, AoRecord::new(val))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_negative_wait_fails_the_connect_even_when_the_pv_is_there() {
    let port = server_with_ao("TST:AO", 1.5).await;

    // The PV exists and answers — a positive wait reads it.
    let (out, _, code) = caget(port, &["-w", "1", "TST:AO"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("1.5"), "baseline read: {out:?}");

    // The SAME PV, with an already-expired deadline: C never waits.
    let (out, err, code) = caget(port, &["-w", "-1", "TST:AO"]).await;
    assert_eq!(code, 1, "C exits 1 (tool_lib.c:628-638)");
    assert_eq!(
        err.trim(),
        "Channel connect timed out: 'TST:AO' not found.",
        "C's connect diagnostic"
    );
    assert!(out.is_empty(), "no value line may be printed: {out:?}");

    // A later, POSITIVE `-w` wins the getopt race and the read succeeds again
    // (R13-17): the expired state is a value, not a latch.
    let (out, _, code) = caget(port, &["-w", "-1", "-w", "1", "TST:AO"]).await;
    assert_eq!(code, 0, "the last -w wins");
    assert!(out.contains("1.5"));
}

/// The three states of C's `caTimeout`, at the boundary. Spelled with
/// `Duration::ZERO` rather than `cli::EXPIRED_TIMEOUT` so the assertion is
/// about the BEHAVIOUR — an expired deadline — and not about the name.
#[test]
fn zero_is_forever_and_negative_is_expired() {
    assert_eq!(
        timeout_duration(-1.0),
        Duration::ZERO,
        "a negative deadline is already in the past"
    );
    assert_eq!(timeout_duration(f64::NEG_INFINITY), Duration::ZERO);
    assert_eq!(
        timeout_duration(0.0),
        INDEFINITE_TIMEOUT,
        "-w 0 waits forever"
    );
    assert_eq!(timeout_duration(2.5).as_secs_f64(), 2.5);
}
