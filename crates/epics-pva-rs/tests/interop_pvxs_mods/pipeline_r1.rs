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
//! Skip behaviour: if `softIocPVX` is not present under
//! `~/codes/pvxs/bin/<arch>/`, the test prints a SKIP line and
//! returns OK so a CI host without pvxs built doesn't fail.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{
    DropChild, SOFT_IOC_PVX, pick_localhost_port, pvxs_command, pvxs_dbd_dir, require_pvxs,
};

use std::io::Write;
use std::process::Stdio;
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
    let Some(ioc_bin) = require_pvxs(SOFT_IOC_PVX) else {
        return;
    };
    let dbd = pvxs_dbd_dir().join("softIocPVX.dbd");
    if !dbd.is_file() {
        eprintln!("SKIP: dbd file missing: {dbd:?}");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("counter.db");
    std::fs::File::create(&db_path)
        .and_then(|mut f| f.write_all(COUNTER_DB.as_bytes()))
        .expect("write db");
    let stderr_path = dir.path().join("ioc.stderr");
    let stdout_path = dir.path().join("ioc.stdout");

    let port = pick_localhost_port();

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
        // Stop softIocPVX from also fighting for CA port 5064 on
        // hosts where another IOC owns it.
        .env("EPICS_CAS_SERVER_PORT", "0")
        .env("EPICS_CA_SERVER_PORT", "0")
        // The signal we depend on.
        .env("PVXS_LOG", "pvxs.tcp.setup=DEBUG")
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ));

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: failed to spawn softIocPVX: {e}");
            return;
        }
    };
    let _ioc = DropChild { child };
    // Wait until the IOC's PVA port is listening (poll up to 5s).
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut bound = false;
    for _ in 0..50 {
        if std::net::TcpStream::connect_timeout(&server_addr, Duration::from_millis(100)).is_ok() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !bound {
        eprintln!("SKIP: softIocPVX did not bind {server_addr} within 5s");
        return;
    }

    // Build a Rust client pinned to the IOC and ask for pipeline.
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .pipeline_size(4)
        .server_addr(server_addr)
        .build();

    let events = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let events_cb = events.clone();
    let handle = client
        .pvmonitor_handle(
            "R1:CNT",
            move |_desc, _v| {
                events_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            |_| {},
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
    assert!(
        events.load(std::sync::atomic::Ordering::Relaxed) >= 2,
        "Rust client did not receive 2 events within 3s — IOC alive but monitor stuck"
    );
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
