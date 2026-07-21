//! Regression tests (R13-16): only a VALID `-0<base>` re-enters the `type`
//! race against `-d`.
//!
//! C's `-0` case scans the base and — under `if (outType != dec)` only —
//! assigns BOTH the integer base and the request type (`caget.c:497-503`):
//!
//! ```c
//! default : outType = dec;  fprintf(stderr, "Invalid argument ...");
//! }
//! if (outType != dec) {
//!   if (opt == '0') { type = DBR_LONG; outTypeI = outType; }
//!   ...
//! ```
//!
//! So an invalid `-0` warns and assigns NOTHING: it is the last `-0`, but not
//! the last `-0` that touched `type`. Observed on the compiled C `caget`
//! (EPICS 7.0.10.1-DEV) against an `ao` holding 1.5:
//!
//! ```text
//! caget -0x -d DBR_DOUBLE -0q TST:AO   C: Request type: DBR_DOUBLE, Value 1.5
//!                                     RS (pre-fix): Request type: DBR_LONG
//! ```
//!
//! The port raced `-d` against the index of the last `-0`, valid or not.
//!
//! These drive the REAL `caget-rs` binary against an in-process `CaServer`, so
//! they pin the tool's printed bytes rather than any internal resolver.

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

use std::process::Command;

use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// The `Request type:` and `Value:` lines of `caget -d`'s report, joined.
///
/// The CA environment goes to the CHILD only (never via `set_var`). The spawn
/// goes through `spawn_blocking` because the server runs as a task on this
/// runtime: blocking a worker on `output()` would starve it.
async fn report(port: u16, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("Request type:")
                .or_else(|| l.strip_prefix("Value:"))
                .map(|v| format!("{} {}", &l[..l.find(':').unwrap() + 1], v.trim()))
        })
        .collect::<Vec<_>>()
        .join(" ")
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
async fn only_a_valid_zero_base_re_enters_the_dbr_type_race() {
    let port = server_with_ao("TST:AO", 1.5).await;
    let report = |args: &'static [&'static str]| report(port, args);

    // The invalid `-0q` is the LAST `-0` but not the last one to ASSIGN, so
    // `-d` still holds `type`. This is the case the port got wrong.
    assert_eq!(
        report(&["-0x", "-d", "DBR_DOUBLE", "-0q", "TST:AO"]).await,
        "Request type: DBR_DOUBLE Value: 1.5",
        "`-0q` is guarded out of the assignment (caget.c:497-503)"
    );
    // A VALID `-0` after `-d` does reclaim `type`.
    assert_eq!(
        report(&["-0x", "-d", "DBR_DOUBLE", "-0b", "TST:AO"]).await,
        "Request type: DBR_LONG Value: 1",
        "`-0b` assigned after `-d`"
    );
    // An invalid `-0` before the valid one changes nothing either way.
    assert_eq!(
        report(&["-0q", "-0x", "-d", "DBR_DOUBLE", "TST:AO"]).await,
        "Request type: DBR_DOUBLE Value: 1.5"
    );
    assert_eq!(
        report(&["-d", "DBR_DOUBLE", "-0q", "-0x", "TST:AO"]).await,
        "Request type: DBR_LONG Value: 0x1"
    );
    // No `-0` ever assigned: `type` is untouched by `-0` entirely.
    assert_eq!(
        report(&["-0q", "-d", "DBR_DOUBLE", "TST:AO"]).await,
        "Request type: DBR_DOUBLE Value: 1.5"
    );
    // `-d` repeats too, and the LAST `-d` is the one racing the last valid `-0`.
    assert_eq!(
        report(&["-d", "DBR_DOUBLE", "-0x", "-d", "DBR_FLOAT", "TST:AO"]).await,
        "Request type: DBR_FLOAT Value: 1.5"
    );
    // Baselines, both directions of the race.
    assert_eq!(
        report(&["-0x", "-d", "DBR_DOUBLE", "TST:AO"]).await,
        "Request type: DBR_DOUBLE Value: 1.5"
    );
    assert_eq!(
        report(&["-d", "DBR_DOUBLE", "-0x", "TST:AO"]).await,
        "Request type: DBR_LONG Value: 0x1"
    );
}
