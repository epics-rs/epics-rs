//! Regression tests (R13-22): caput VALIDATES the value before it reads and
//! prints `Old :`.
//!
//! C's `main` does the enum-menu read (`caput.c:455-465`), then builds and
//! validates every write value (`:466-530`) — and every failure in that block
//! `return 1`s on the spot. Only after it survives does the readback run:
//!
//! ```c
//! if (format != terse) {               /* caput.c:532-535 */
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

#![cfg(tokio_backend)]

use std::process::Command;

use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

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

/// The server TAKES its port by binding it (`.port(0)` → read back
/// `udp_port()`); nothing probes a port and hands the number on.
async fn server_with_mbbo(pv: &'static str) -> u16 {
    let mut rec = MbboRecord::new(0);
    rec.zrst = "Zero".into();
    rec.onst = "One".into();
    rec.twst = "Two".into();
    let server = CaServer::builder()
        .port(0)
        .record(pv, rec)
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
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

/// R13-23: an enum index at or past the end of the menu WARNS but still puts.
///
/// C prints `"Warning: enum index value '%s' may be too large.\n"` at both
/// places an ENUM value becomes a number — the `-n` path (caput.c:477-479) and
/// the string path's numeric fallback (caput.c:505-507) — and neither site
/// `return`s, unlike every other diagnostic in that block. The value is written
/// regardless, so the tool exits 0 and prints its `Old :` / `New :` lines.
///
/// The menu here has three states (Zero/One/Two), so `no_str == 3` and index 3
/// is the first that warns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_out_of_range_enum_index_warns_and_still_puts() {
    let port = server_with_mbbo("TST:MBBO").await;
    let warning = "Warning: enum index value '99' may be too large.";

    // -n path (caput.c:477-479).
    let (out, err, code) = caput(port, &["-n", "TST:MBBO", "99"]).await;
    assert_eq!(code, 0, "the warning is not fatal: C puts the value anyway");
    assert!(err.contains(warning), "-n path stderr was: {err:?}");
    assert!(out.contains("Old : "), "the put still runs: {out:?}");
    assert!(out.contains("New : "), "the put still runs: {out:?}");

    // Numeric fallback of the string path (caput.c:505-507): '99' is not a menu
    // name, so it is retried as an index — and warns from the other site.
    let (out, err, code) = caput(port, &["TST:MBBO", "99"]).await;
    assert_eq!(
        code, 0,
        "the warning is not fatal on the string path either"
    );
    assert!(err.contains(warning), "string-path stderr was: {err:?}");
    assert!(out.contains("New : "), "the put still runs: {out:?}");

    // The boundary: `dbuf[i] >= no_str`. With three states, 2 is in range and 3
    // is not — the first index past the end is the first to warn.
    let (_, err, code) = caput(port, &["-n", "TST:MBBO", "2"]).await;
    assert_eq!(code, 0);
    assert!(
        !err.contains("may be too large"),
        "index 2 is the last valid state of a 3-state menu: {err:?}"
    );
    let (_, err, code) = caput(port, &["-n", "TST:MBBO", "3"]).await;
    assert_eq!(code, 0);
    assert!(
        err.contains("Warning: enum index value '3' may be too large."),
        "index 3 is one past the end of a 3-state menu: {err:?}"
    );

    // A menu NAME is never compared against `no_str` — C only tests the numeric
    // `dbuf[i]`, so the valid-name path stays silent.
    let (_, err, code) = caput(port, &["TST:MBBO", "Two"]).await;
    assert_eq!(code, 0);
    assert!(!err.contains("may be too large"), "name path: {err:?}");
}
