//! Rust CA client ↔ C softIoc interop.
//!
//! These tests spawn a real `softIoc` (EPICS base reference IOC) and
//! exercise the Rust client against it. Skipped when softIoc is not
//! available (e.g. CI without EPICS install).
//!
//! Run with: `cargo test -p epics-ca-rs --test interop_rust_client_c_ioc`
//!
//! Host/tokio-only. The client side is the async `CaClient`, which needs a
//! tokio reactor that `rtems-exec-model` deliberately does not start; measured
//! under `--profile interop`, all four tests fail there with "there is no
//! reactor running". The C `softIoc` side is fine — this is the same
//! by-design async-client exclusion as the rest of the gated client suites,
//! not an interop defect.

#![cfg(not(feature = "rtems-exec-model"))]

mod common;

use std::time::Duration;

use common::{require_tool, spawn_softioc};
use epics_base_rs::types::EpicsValue;
use serial_test::serial;

const TEST_DB: &str = "
record(ai, \"TEST:AI\") {
    field(VAL, \"42.0\")
    field(EGU, \"V\")
    field(PREC, \"3\")
}
record(stringin, \"TEST:STR\") {
    field(VAL, \"hello\")
}
record(longout, \"TEST:LOUT\") {
    field(VAL, \"0\")
}
record(waveform, \"TEST:WAV\") {
    field(NELM, \"10\")
    field(FTVL, \"DOUBLE\")
}
";

fn set_client_env(addr_list: &str, port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", addr_list);
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_can_caget_from_softioc() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(TEST_DB) else {
        eprintln!("SKIP: spawn_softioc failed");
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("connect");
    let (_, value) = ch
        .get_with_timeout(Duration::from_secs(3))
        .await
        .expect("caget");
    let v = value.to_f64().expect("scalar");
    assert!((v - 42.0).abs() < 0.001, "got {v}, expected 42.0");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_can_caput_to_softioc() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(TEST_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("connect");
    eprintln!("test: connected, calling put");
    let put_res = ch.put(&EpicsValue::Long(1234)).await;
    eprintln!("test: put returned {:?}", put_res);
    put_res.expect("put");

    // Read back via Rust client to verify the IOC accepted the value.
    let (_, value) = ch
        .get_with_timeout(Duration::from_secs(3))
        .await
        .expect("readback");
    assert_eq!(value.to_f64().unwrap_or(0.0) as i64, 1234);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_monitors_softioc_changes() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(TEST_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("connect");

    let mut monitor = ch.subscribe().await.expect("subscribe");

    // Drain the initial snapshot (libca-style first-event).
    let _ = tokio::time::timeout(Duration::from_secs(2), monitor.recv()).await;

    // Drive value changes from a separate caput task.
    let addr_list = ioc.ca_addr_list();
    let server_port = ioc.udp_port;
    tokio::task::spawn_blocking(move || {
        for v in [10, 20, 30] {
            let _ = common::run_caput(&addr_list, server_port, "TEST:LOUT", &v.to_string());
            std::thread::sleep(Duration::from_millis(150));
        }
    });

    let mut last_seen = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), monitor.recv()).await {
            Ok(Some(Ok(snap))) => {
                last_seen = snap.value.to_f64().unwrap_or(0.0) as i64;
                if last_seen == 30 {
                    break;
                }
            }
            _ => continue,
        }
    }
    assert_eq!(last_seen, 30, "monitor never converged on final value");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_handles_softioc_restart() {
    if !require_tool("softIoc") {
        return;
    }

    // First IOC instance.
    let Some(ioc1) = spawn_softioc(TEST_DB) else {
        return;
    };
    let addr = ioc1.ca_addr_list();
    let port = ioc1.udp_port;
    set_client_env(&addr, port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("first connect");

    // Bring down the IOC, then immediately stand up a new one on the
    // same UDP port. The client should re-search and reconnect via
    // beacon-anomaly + reconnect logic.
    drop(ioc1);
    std::thread::sleep(Duration::from_secs(1));

    // Spawning a *new* IOC on the same port simulates a process restart.
    // We can't reuse spawn_softioc because it picks fresh ports each time;
    // re-bind via direct softIoc invocation with the same port.
    let dir = tempfile::tempdir().expect("temp");
    let db = dir.path().join("test.db");
    std::fs::write(&db, TEST_DB).expect("db");
    let mut child = std::process::Command::new("softIoc")
        .arg("-S")
        .arg("-d")
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CAS_SERVER_PORT", port.to_string())
        .env("EPICS_CA_REPEATER_PORT", "5165")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn second IOC");
    std::mem::forget(dir);

    // Reconnection should complete within ~10s (reconnect lane backoff).
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok((_, value)) = ch.get_with_timeout(Duration::from_secs(2)).await
                && (value.to_f64().unwrap_or(0.0) - 42.0).abs() < 0.001
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;

    let _ = child.kill();
    let _ = child.wait();

    assert!(result.is_ok(), "did not reconnect after IOC restart");
}
