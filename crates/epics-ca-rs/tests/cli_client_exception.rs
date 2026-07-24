//! Regression test (R13-24): a server-rejected put prints libca's
//! `CA.Client.Exception` block.
//!
//! `caput` without `-c` sends a plain `CA_PROTO_WRITE`, which carries no
//! completion callback. When the server refuses it, rsrv answers with a
//! `CA_PROTO_ERROR`; libca's exception table routes that to `cac::writeExcep`
//! → `oldChannelNotify::writeException` (`cac.cpp:1049-1061`), and with no user
//! handler installed the DEFAULT handler
//! (`ca_client_context.cpp:289-349` → `vSignal`) prints the block on stderr.
//! The tool's exit status is untouched — `ca_pend_io` saw nothing wrong.
//! Observed head-to-head against a live C softIoc (EPICS 7.0.10.1-DEV):
//!
//! ```text
//! caput TST:LO abc
//!   C  (stdout): Old : / New : , exit 0
//!      (stderr): CA.Client.Exception...............................................
//!                    Warning: "Channel write request failed"
//!                    Context: "op=1, channel=TST:LO, type=DBR_STRING, count=1, ctx="TST:LO""
//!                    Source File: ../oldChannelNotify.cpp line 159
//!                    Current Time: Mon Jul 13 2026 09:25:31.803490065
//!                ..................................................................
//!   RS (stderr): (nothing)                                              (pre-fix)
//! ```
//!
//! `dispatch_exception` dropped every exception that had no registered handler
//! on the floor, so the operator saw a `New :` line echoing the UNCHANGED value
//! and no hint that the write had been refused.
//!
//! The exact text of the block is pinned as a unit test beside the renderer
//! (`client::types::default_exception_tests`); this test pins the WIRING —
//! that a `CA_PROTO_ERROR` for a plain write really does travel
//! transport → coordinator → default handler → the tool's stderr.
//!
//! The rejection here is a write to a read-only field: the one put the
//! in-process `CaServer` refuses. (Its ECA differs from rsrv's for this case —
//! reported separately; the block's shape and routing are what this test is
//! about.)

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

use std::process::Command;

use epics_base_rs::server::records::longout::LongoutRecord;
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
async fn server_with_longout(pv: &'static str, val: i32) -> u16 {
    let server = CaServer::builder()
        .port(0)
        .record(pv, LongoutRecord::new(val))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_server_rejected_put_prints_the_client_exception_block() {
    let port = server_with_longout("TST:LO", 200).await;

    let (out, err, code) = caput(port, &["TST:LO.RTYP", "foo"]).await;

    // C's exit status and stdout are unchanged by the rejection: the plain
    // write has no completion status for the tool to look at.
    assert_eq!(code, 0, "C exits 0 (caput.c:558-577 never sees the error)");
    assert!(out.contains("Old : "), "{out:?}");
    assert!(out.contains("New : "), "{out:?}");

    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some("CA.Client.Exception..............................................."),
        "stderr must open with libca's block: {err:?}"
    );
    assert!(
        lines
            .get(1)
            .is_some_and(|l| l.starts_with("    Warning: \"")),
        "severity + ca_message(status): {err:?}"
    );
    // `op=1` is CA_OP_PUT, the channel is the one the write was on, and the
    // type/count come from the echoed request header. `ctx` is whatever
    // diagnostic the server put in the error payload (rsrv sends the record
    // name; the Rust server sends the full channel name).
    assert_eq!(
        lines.get(2).copied(),
        Some(
            "    Context: \"op=1, channel=TST:LO.RTYP, type=DBR_STRING, count=1, \
             ctx=\"TST:LO.RTYP\"\""
        ),
        "C's channel-scoped context (ca_client_context.cpp:342-347): {err:?}"
    );
    assert_eq!(
        lines.get(3).copied(),
        Some("    Source File: ../oldChannelNotify.cpp line 159"),
        "libca's raising site: {err:?}"
    );
    assert!(
        lines
            .get(4)
            .is_some_and(|l| l.starts_with("    Current Time: ")),
        "{err:?}"
    );
    assert_eq!(
        lines.get(5).copied(),
        Some("..................................................................")
    );
    assert_eq!(lines.len(), 6, "nothing else on stderr: {err:?}");
}

/// A put the server ACCEPTS raises nothing — the block is not a per-put
/// decoration, and a caget on the same circuit stays silent too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_accepted_put_prints_no_block() {
    let port = server_with_longout("TST:LO2", 200).await;

    let (out, err, code) = caput(port, &["TST:LO2", "7"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("New : "), "{out:?}");
    assert!(
        !err.contains("CA.Client.Exception"),
        "a successful put is silent: {err:?}"
    );
}
