//! Regression tests (W10-B2): `-e` / `-f` / `-g` — the LAST VALID occurrence
//! in getopt order wins.
//!
//! C's getopt loop shares one body across `case 'e': case 'f': case 'g':` and
//! that body rewrites a single `dblFormatStr` (`caget.c:470-484`,
//! `camonitor.c:310-324`):
//!
//! ```c
//! if (digits>=0 && digits<=VALID_DOUBLE_DIGITS)
//!     sprintf(dblFormatStr, "%%-.%d%c", digits, opt);   /* only when VALID */
//! ```
//!
//! So the three letters do not have a precedence — whichever scanned valid
//! most recently wins — and an invalid occurrence leaves the previous format
//! standing. Observed on the compiled C `caget` (EPICS 7.0.10.1-DEV) against
//! an `ao` holding 1.5:
//!
//! ```text
//! caget -e 2 -f 4  TST:AO   C: 1.5000      RS (pre-fix): 1.50e+00
//! caget -e 5 -g 2  TST:AO   C: 1.5         RS (pre-fix): 1.50000e+00
//! caget -f 4 -g 2  TST:AO   C: 1.5         RS (pre-fix): 1.5000
//! caget -f 4 -e 99 TST:AO   C: 1.5000      RS (pre-fix): 1.5000    (matched)
//! ```
//!
//! The port resolved `e` → else-if `f` → else-if `g`, a fixed precedence with
//! no C counterpart.
//!
//! These drive the REAL `caget-rs` binary against an in-process `CaServer`, so
//! they pin the tool's printed bytes rather than any internal resolver — which
//! is what makes them a negative control that survives an API change.

#![cfg(tokio_backend)]

use std::process::Command;

use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Run `caget-rs` against the server on `port`, returning its stdout.
///
/// The CA environment is passed to the CHILD only (never via `set_var`), so
/// the tool finds the in-process server without touching this process's
/// environment. The spawn goes through `spawn_blocking` because the server
/// runs as a task on this runtime: blocking a worker on `output()` would
/// starve it and every search would time out.
async fn caget(port: u16, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let shown = args.clone();
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
    assert!(
        out.status.success(),
        "caget-rs {shown:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The value column of a `caget-rs` line (`<name> <value>`).
async fn value(port: u16, args: &[&str]) -> String {
    caget(port, args)
        .await
        .split_whitespace()
        .next_back()
        .expect("a value column")
        .to_string()
}

/// One `ao` holding 1.5, the record the C observations above were taken on.
///
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
async fn float_format_is_the_last_valid_of_e_f_g_in_getopt_order() {
    let port = server_with_ao("TST:AO", 1.5).await;
    let value = |args: &'static [&'static str]| value(port, args);

    // Baselines: each letter alone.
    assert_eq!(value(&["-e", "3", "TST:AO"]).await, "1.500e+00");
    assert_eq!(value(&["-f", "3", "TST:AO"]).await, "1.500");
    assert_eq!(value(&["-g", "3", "TST:AO"]).await, "1.5");

    // The last VALID one wins — in command-line order, across the letters.
    assert_eq!(
        value(&["-e", "2", "-f", "4", "TST:AO"]).await,
        "1.5000",
        "C: -f is last, so %.4f (caget.c:470-484)"
    );
    assert_eq!(value(&["-e", "5", "-g", "2", "TST:AO"]).await, "1.5");
    assert_eq!(value(&["-f", "4", "-g", "2", "TST:AO"]).await, "1.5");
    assert_eq!(
        value(&["-g", "2", "-e", "5", "TST:AO"]).await,
        "1.50000e+00",
        "and no g > e precedence either"
    );

    // An invalid occurrence never reaches the sprintf, so it cannot clear an
    // earlier valid one.
    assert_eq!(
        value(&["-f", "4", "-e", "99", "TST:AO"]).await,
        "1.5000",
        "out of range: dblFormatStr keeps %.4f"
    );
    assert_eq!(
        value(&["-f", "4", "-g", "abc", "TST:AO"]).await,
        "1.5000",
        "unscannable: same"
    );

    // A repeat of one letter folds the same way (R13-17).
    assert_eq!(
        value(&["-e", "2", "-e", "6", "TST:AO"]).await,
        "1.500000e+00"
    );
    assert_eq!(
        value(&["-e", "2", "-f", "4", "-e", "6", "TST:AO"]).await,
        "1.500000e+00",
        "interleaved repeats: still the last valid one"
    );
}
