//! Load and failure-mode tests against a real C softIoc.
//!
//! These exercise the Rust client under conditions that would expose
//! state-machine / queue / lifecycle bugs:
//!
//! - Many concurrent channels
//! - Rapid create/clear cycles
//! - Burst of writes
//! - Slow consumer behind monitor coalescing
//!
//! Skipped when softIoc is unavailable.

// RTEMS-EXEC-MODEL-ALLOW(4): not run by the default nextest profile (default-filter excludes it).

mod common;

use std::time::Duration;

use common::{require_tool, spawn_softioc};
use epics_base_rs::types::EpicsValue;
use serial_test::serial;

const STRESS_DB: &str = "
record(longout, \"S:0\") { field(VAL, \"0\") }
record(longout, \"S:1\") { field(VAL, \"0\") }
record(longout, \"S:2\") { field(VAL, \"0\") }
record(longout, \"S:3\") { field(VAL, \"0\") }
record(longout, \"S:4\") { field(VAL, \"0\") }
record(longout, \"S:5\") { field(VAL, \"0\") }
record(longout, \"S:6\") { field(VAL, \"0\") }
record(longout, \"S:7\") { field(VAL, \"0\") }
record(longout, \"S:8\") { field(VAL, \"0\") }
record(longout, \"S:9\") { field(VAL, \"0\") }
record(ai, \"COUNTER\") { field(VAL, \"0\") }
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
async fn many_concurrent_channels_connect() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(STRESS_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");

    // Open all 10 stress PVs concurrently.
    let channels: Vec<_> = (0..10)
        .map(|i| client.create_channel(&format!("S:{i}")))
        .collect();

    // Wait for every one to connect.
    let mut connected = 0;
    for ch in &channels {
        if ch.wait_connected(Duration::from_secs(5)).await.is_ok() {
            connected += 1;
        }
    }
    assert_eq!(connected, 10, "not all channels connected");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rapid_create_drop_cycles() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(STRESS_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");

    // Cycle: create channel, wait_connected, drop. 50 rounds.
    // Verifies no leak in coordinator state and that ClearChannel is
    // sent reliably.
    for round in 0..50 {
        let pv = format!("S:{}", round % 10);
        let ch = client.create_channel(&pv);
        ch.wait_connected(Duration::from_secs(3))
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        drop(ch);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn burst_of_writes_completes() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(STRESS_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("S:0");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("connect");

    // Fire 100 writes in tight succession.
    for i in 0..100i32 {
        ch.put(&EpicsValue::Long(i)).await.expect("put");
    }

    // Final readback should match the last written value.
    let (_, value) = ch
        .get_with_timeout(Duration::from_secs(3))
        .await
        .expect("readback");
    assert_eq!(value.to_f64().unwrap_or(0.0) as i64, 99);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn monitor_keeps_up_with_high_update_rate() {
    if !require_tool("softIoc") {
        return;
    }
    let Some(ioc) = spawn_softioc(STRESS_DB) else {
        return;
    };
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("S:0");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("connect");

    let mut monitor = ch.subscribe().await.expect("subscribe");

    // Drive 200 updates as fast as the IOC will accept.
    let writer_ch = client.create_channel("S:0");
    writer_ch
        .wait_connected(Duration::from_secs(5))
        .await
        .expect("writer connect");

    let writer = tokio::spawn(async move {
        for v in 0..200i32 {
            let _ = writer_ch.put(&EpicsValue::Long(v)).await;
        }
    });

    // Drain monitor until we see the final value or timeout.
    let mut last: i64 = -1;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), monitor.recv()).await {
            Ok(Some(Ok(snap))) => {
                last = snap.value.to_f64().unwrap_or(0.0) as i64;
                if last >= 199 {
                    break;
                }
            }
            _ => continue,
        }
    }
    let _ = writer.await;
    // Coalescing can drop intermediate values, but the LAST value must arrive.
    assert!(
        last >= 199,
        "monitor did not converge on final value (last={last})"
    );
}
