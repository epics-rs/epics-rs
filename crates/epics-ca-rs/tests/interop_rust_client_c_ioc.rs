//! Rust CA client ↔ C softIoc interop.
//!
//! These tests spawn a real `softIoc` (EPICS base reference IOC) and
//! exercise the Rust client against it. Skipped when softIoc is not
//! available (e.g. CI without EPICS install).
//!
//! Run with: `cargo test -p epics-ca-rs --test interop_rust_client_c_ioc`
//!
//! Both backends: the "no reactor running" this file used to record came from
//! the CA stack minting its spawn capability from the seam `Reactor` on a
//! build whose listeners are tokio's, not from anything the C `softIoc` side
//! does.

#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(4): measured, not argued — every case here is
// `#[ignore]`d, and all four pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread
// cargo nextest run --profile interop -p epics-ca-rs --run-ignored all`.

mod common;

use std::time::Duration;

use common::{require_tool, spawn_softioc, spawn_softioc_on};
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
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect");
    let (_, value) = ch
        .get_with_timeout(budget::FACT_BUDGET)
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
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect");
    eprintln!("test: connected, calling put");
    let put_res = ch.put(&EpicsValue::Long(1234)).await;
    eprintln!("test: put returned {:?}", put_res);
    put_res.expect("put");

    // Read back via Rust client to verify the IOC accepted the value.
    let (_, value) = ch
        .get_with_timeout(budget::FACT_BUDGET)
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
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(budget::FACT_BUDGET)
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
    let deadline = tokio::time::Instant::now() + budget::FACT_BUDGET;
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
    let ioc1 = spawn_softioc(TEST_DB);
    let addr = ioc1.ca_addr_list();
    let port = ioc1.udp_port;
    set_client_env(&addr, port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("first connect");

    // Bring down the IOC, then immediately stand up a new one on the
    // same UDP port. The client should re-search and reconnect via
    // beacon-anomaly + reconnect logic.
    drop(ioc1);
    std::thread::sleep(Duration::from_secs(1));

    // Spawning a *new* IOC on the same port simulates a process restart, so
    // the number IS the subject here and a fresh candidate would be a
    // different test. `spawn_softioc_on` therefore fails on a steal rather
    // than retrying — and it waits for the IOC's own "up" line, where this
    // used to hand the reconnect budget a child that might never have bound.
    let ioc2 = spawn_softioc_on(TEST_DB, port);

    // Reconnection should complete within ~10s (reconnect lane backoff).
    let result = tokio::time::timeout(budget::FACT_BUDGET, async {
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

    let console = ioc2.console();
    drop(ioc2);

    assert!(
        result.is_ok(),
        "did not reconnect after IOC restart; the second IOC said:\n{console}"
    );
}

use common::budget;
