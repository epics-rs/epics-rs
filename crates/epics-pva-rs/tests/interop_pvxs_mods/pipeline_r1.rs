//! Interop: Rust client (pipeline_size > 0) → pvxs
//! `softIocPVX` server, asserting that pvxs sees the pipeline
//! option on the wire.
//!
//! Previously the Rust client honoured `PvaClientBuilder::pipeline_size`
//! locally but never put `record._options.pipeline` into the
//! pvRequest, so pvxs ran the monitor in non-pipelined mode and
//! we lost flow control. pvxs's `servermon.cpp:587` logs
//! `Client … Monitor INIT pipeline ioid=…` at DEBUG level (channel
//! `pvxs.tcp.setup`) iff `op->pipeline` is true after parsing
//! `record._options.pipeline`. We use that log line as the
//! authoritative wire-level signal.
//!
//! If `softIocPVX` is not present under the resolved pvxs tree's
//! `bin/<arch>/`, the test fails.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{
    SOFT_IOC_PVX, pick_localhost_port, pvxs_command, pvxs_dbd_dir, require_pvxs,
};

use std::io::Write;
use std::time::Duration;

const COUNTER_DB: &str = r#"
record(calc, "R1:CNT") {
    field(SCAN, ".1 second")
    field(CALC, "A+1")
    field(INPA, "R1:CNT.VAL")
    field(VAL,  "0")
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r1_pipeline_option_visible_to_pvxs_server() {
    let ioc_bin = require_pvxs(SOFT_IOC_PVX);
    let dbd = pvxs_dbd_dir().join("softIocPVX.dbd");
    assert!(dbd.is_file(), "dbd file missing: {dbd:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("counter.db");
    std::fs::File::create(&db_path)
        .and_then(|mut f| f.write_all(COUNTER_DB.as_bytes()))
        .expect("write db");
    let stderr_path = dir.path().join("ioc.stderr");
    let stdout_path = dir.path().join("ioc.stdout");

    let logs = super::interop_helpers::PvxsLogs {
        out: stdout_path.clone(),
        err: stderr_path.clone(),
    };
    let build = || {
        // Fresh per attempt: a retry after `port_collision` must not name the
        // number that was just lost.
        let port = pick_localhost_port();
        let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let ca = named_port::free_for_tcp_and_udp();
        let mut cmd = pvxs_command(&ioc_bin);
        cmd.arg("-D")
            .arg(&dbd)
            .arg("-d")
            .arg(&db_path)
            .arg("-S")
            // PVA port — what we want to control.
            .env("EPICS_PVAS_SERVER_PORT", port.to_string())
            .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
            // A named CA port: `0` falls back to 5064 (`envSubr.c:410`),
            // and a lost one is rerun through `port_collision`.
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_SERVER_PORT", ca.to_string())
            .env("EPICS_CA_SERVER_PORT", ca.to_string())
            // The signal we depend on. `pvxs.tcp.*` rather than
            // `pvxs.tcp.setup` alone so the failure path below also captures
            // the server's per-reply flow-control decisions from `connio`
            // ("Skip reply sch=.. w=..", "IOID .. acks ..") — a stalled
            // credit window and a subscription that was never started look
            // identical from the client side.
            .env("PVXS_LOG", "pvxs.tcp.*=DEBUG");
        (cmd, server_addr)
    };
    let (_ioc, server_addr) =
        match super::interop_helpers::start_pvxs(&logs, build, super::interop_helpers::ioc_ready) {
            Ok(up) => up,
            Err(text) => panic!(
                "softIocPVX did not come up. A spawned IOC that \
             never comes up is a broken reference, not a missing one.\n{text}"
            ),
        };

    // Build a Rust client pinned to the IOC and ask for pipeline.
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .pipeline_size(4)
        .server_addr(server_addr)
        .build();

    // `pvmonitor_handle` awaits only channel creation, so its success
    // says nothing about the MONITOR INIT. The connection transitions are
    // recorded alongside the event count because the two stalls have
    // different causes and the message has to name which one happened:
    // no `Connected` means INIT/START never completed, `Connected` with
    // no data means the server accepted the subscription and sent nothing.
    let events = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let events_cb = events.clone();
    let conn = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let conn_cb = conn.clone();
    let handle = client
        .pvmonitor_handle(
            "R1:CNT",
            move |_desc, _v| {
                events_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            move |e| conn_cb.lock().unwrap().push(format!("{e:?}")),
        )
        .await
        .expect("subscribe");

    // Wait for the negotiation + at least 2 events (which proves the
    // INIT round-trip completed). 3s max budget — SCAN is 0.1s.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline
        && events.load(std::sync::atomic::Ordering::Relaxed) < 2
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let seen = events.load(std::sync::atomic::Ordering::Relaxed);
    if seen < 2 {
        let transitions = conn.lock().unwrap().join(", ");
        let err = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let out = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        panic!(
            "Rust client received {seen} of 2 events within 3s — IOC alive but \
             monitor stuck.\nconnection transitions: [{transitions}]\n\
             pvxs stdout:\n{out}\npvxs stderr:\n{err}"
        );
    }
    handle.stop();

    // Give the pvxs server a moment to flush its log buffer.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let log_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    // pvxs logs `Monitor INIT pipeline ioid=N` (with " pipeline") iff
    // `op->pipeline == true` (servermon.cpp:587).
    assert!(
        log_text.contains("Monitor INIT pipeline ioid="),
        "Regression: pvxs server did not log `Monitor INIT pipeline …`. \
         The pipeline option was either absent from pvRequest or pvxs failed \
         to parse it as true. Full pvxs stderr:\n{log_text}",
    );
}
