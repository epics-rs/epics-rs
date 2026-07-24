//! Regression tests (R13-19): camonitor's field separator sits where C puts
//! it — a PREFIX of the value, not a suffix of the timestamp.
//!
//! C `print_time_val_sts` (`tool_lib.c:515-519`) prints the name, then an
//! UNCONDITIONAL separator, then whatever the timestamp block emitted — which
//! is NOTHING under `-t n`, where neither `tsSrcServer` nor `tsSrcClient` is
//! set — and the value then brings its own separator, because C's value loop
//! writes every item as `printf("%c%s", fieldSeparator, item)`
//! (`tool_lib.c:481-489`).
//!
//! So a `-t n` line carries TWO adjacent separators. Observed on the compiled
//! C `camonitor` (EPICS 7.0.10.1-DEV) against an `ao` holding 1.5:
//!
//! ```text
//! camonitor -F , -t n TST:AO    C: TST:AO,,1.5,,
//!                              RS: TST:AO,1.5,,     (pre-fix)
//! ```
//!
//! The port attached the separator to the timestamp and dropped it with the
//! timestamp — a dual meaning C's line shape does not have.

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::LineCollector;
use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// The first line the real `camonitor-rs` prints for `args`.
///
/// camonitor never exits, so this kills it once the leading event is out —
/// and it waits for that event itself (first complete stdout line) rather
/// than sleeping a fixed interval, which raced connect + first event under
/// CI load. The CA environment goes to the CHILD only; the blocking wait
/// goes through `spawn_blocking` so the in-process server keeps running on
/// its own worker.
async fn first_line(port: u16, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let port_env = port.to_string();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_camonitor-rs"))
            .args(&args)
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port_env)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn camonitor-rs");
        let collector = LineCollector::spawn(child.stdout.take().expect("piped stdout"));
        let got = collector.wait_for(Duration::from_secs(10), |text| text.contains('\n'));
        child.kill().expect("kill camonitor-rs");
        let _ = child.wait();
        let text = collector.into_text();
        assert!(got, "camonitor-rs printed no line within 10s; got {text:?}");
        text.lines().next().unwrap_or_default().to_string()
    })
    .await
    .expect("camonitor-rs child joined")
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
    // Process once so the record is DEFINED: a never-processed record carries
    // the initial UDF severity (C `iocInit.c:521-523` — STAT=UDF SEVR=INVALID),
    // which would fill the two alarm columns this test wants empty.
    let mut visited = std::collections::HashSet::new();
    server
        .database()
        .process_record_with_links(pv, &mut visited, 0)
        .await
        .expect("seed process");
    tokio::spawn(async move { server.run().await });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_missing_timestamp_does_not_swallow_the_value_separator() {
    let port = server_with_ao("TST:AO", 1.5).await;

    // `-t n`: name, separator, EMPTY timestamp, separator, value, then the two
    // empty alarm fields.
    assert_eq!(
        first_line(port, &["-F", ",", "-t", "n", "TST:AO"]).await,
        "TST:AO,,1.5,,",
        "C prints the name separator unconditionally (tool_lib.c:517)"
    );

    // With a timestamp the same shape must still hold — one separator each
    // side of the stamp — which is why the fix moves the separator rather than
    // special-casing the empty column.
    let line = first_line(port, &["-F", ",", "TST:AO"]).await;
    let fields: Vec<&str> = line.split(',').collect();
    assert_eq!(fields.len(), 5, "name,ts,value,stat,sevr — got {line:?}");
    assert_eq!(fields[0], "TST:AO");
    assert!(!fields[1].is_empty(), "the stamp column: {line:?}");
    assert_eq!(fields[2], "1.5");
    assert_eq!(
        (fields[3], fields[4]),
        ("", ""),
        "NO_ALARM prints two empties"
    );
}
