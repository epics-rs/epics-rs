//! Regression tests (R13-22): caput VALIDATES the value before it reads and
//! prints `Old :`.
//!
//! C's `main` does the enum-menu read (`caput.c:455-465`), then builds and
//! validates every write value (`:466-530`) — and every failure in that block
//! `return 1`s on the spot. Only after it survives does the readback run:
//!
//! ```c
//! if (format != terse) {               /* caput.c:531-535 */
//!     printf("Old : ");
//!     caget(chs, nPvs, ...);
//! }
//! ```
//!
//! So a rejected value never leaves a readback line behind. Observed on the
//! compiled C `caput` (EPICS 7.0.10.1-DEV) against a live softIoc:
//!
//! ```text
//! caput TST:MBBO Bogus   C: (stdout empty)  stderr: Enum string value 'Bogus' invalid.
//!                       RS: Old : TST:MBBO   Zero                                    (pre-fix)
//! ```
//!
//! The port parsed AFTER the print, so a put that never happened still
//! reported an old value.

use std::process::Command;
use std::time::Duration;

use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
    let p = probe.local_addr().unwrap().port();
    drop(probe);
    p
}

/// (stdout, stderr, exit code) of the real `caput-rs`.
async fn caput(port: u16, args: &[&str]) -> (String, String, i32) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_caput-rs"))
            .args(&args)
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .output()
            .expect("spawn caput-rs")
    })
    .await
    .expect("caput-rs child joined");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

async fn server_with_mbbo(pv: &'static str) -> u16 {
    let port = free_port();
    let mut rec = MbboRecord::new(0);
    rec.zrst = "Zero".into();
    rec.onst = "One".into();
    rec.twst = "Two".into();
    let server = CaServer::builder()
        .port(port)
        .record(pv, rec)
        .build()
        .await
        .expect("build CA server");
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rejected_enum_string_prints_no_old_value() {
    let port = server_with_mbbo("TST:MBBO").await;

    // The menu is live — a VALID string does reach the readback+put.
    let (out, _, code) = caput(port, &["TST:MBBO", "One"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("Old : "), "baseline put: {out:?}");
    assert!(out.contains("New : "), "baseline put: {out:?}");

    // An INVALID one dies in the build step, before `Old : ` can be printed.
    let (out, err, code) = caput(port, &["TST:MBBO", "Bogus"]).await;
    assert_eq!(code, 1);
    assert_eq!(err.trim(), "Enum string value 'Bogus' invalid.");
    assert!(
        out.is_empty(),
        "a rejected put must leave nothing on stdout: {out:?}"
    );
}

/// The same ordering holds for the `-n` (numeric-enum) parse failure — the
/// whole build block precedes the print, not just the enum-string arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_rejected_numeric_enum_prints_no_old_value() {
    let port = server_with_mbbo("TST:MBBO").await;

    let (out, err, code) = caput(port, &["-n", "TST:MBBO", "abc"]).await;
    assert_eq!(code, 1);
    assert!(
        !err.trim().is_empty(),
        "C diagnoses the bad number on stderr"
    );
    assert!(
        out.is_empty(),
        "a rejected put must leave nothing on stdout: {out:?}"
    );
}
