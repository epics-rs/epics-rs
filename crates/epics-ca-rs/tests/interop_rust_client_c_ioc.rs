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

// RTEMS-EXEC-MODEL-ALLOW(6): measured, not argued — every case here is
// `#[ignore]`d, and all six pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread
// cargo nextest run --profile interop -p epics-ca-rs --run-ignored all`.

use std::time::Duration;

use c_ca_tools::{require_tool, spawn_softioc, spawn_softioc_on, spawn_softioc_with_acf};
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
    require_tool("softIoc");
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("connect");
    let (_, value) = ch
        .get_with_timeout(fact_budget::FACT_BUDGET)
        .await
        .expect("caget");
    let v = value.to_f64().expect("scalar");
    assert!((v - 42.0).abs() < 0.001, "got {v}, expected 42.0");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_can_caput_to_softioc() {
    require_tool("softIoc");
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("connect");
    eprintln!("test: connected, calling put");
    let put_res = ch.put(&EpicsValue::Long(1234)).await;
    eprintln!("test: put returned {:?}", put_res);
    put_res.expect("put");

    // Read back via Rust client to verify the IOC accepted the value.
    let (_, value) = ch
        .get_with_timeout(fact_budget::FACT_BUDGET)
        .await
        .expect("readback");
    assert_eq!(value.to_f64().unwrap_or(0.0) as i64, 1234);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn rust_client_monitors_softioc_changes() {
    require_tool("softIoc");
    let ioc = spawn_softioc(TEST_DB);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:LOUT");
    ch.wait_connected(fact_budget::FACT_BUDGET)
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
            let _ = c_ca_tools::run_caput(&addr_list, server_port, "TEST:LOUT", &v.to_string());
            std::thread::sleep(Duration::from_millis(150));
        }
    });

    let mut last_seen = 0;
    let deadline = tokio::time::Instant::now() + fact_budget::FACT_BUDGET;
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
    require_tool("softIoc");

    // First IOC instance.
    let ioc1 = spawn_softioc(TEST_DB);
    let addr = ioc1.ca_addr_list();
    let port = ioc1.udp_port;
    set_client_env(&addr, port);

    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let ch = client.create_channel("TEST:AI");
    ch.wait_connected(fact_budget::FACT_BUDGET)
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
    let result = tokio::time::timeout(fact_budget::FACT_BUDGET, async {
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

/// Stop or continue the IOC process; a stopped softIoc keeps its socket and
/// answers nothing, which is the echo-timeout path, not a circuit close.
fn signal(pid: u32, sig: &str) {
    let ok = std::process::Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .expect("run kill")
        .success();
    assert!(ok, "kill {sig} {pid} failed");
}

/// One unresponsive → responsive cycle against a softIoc granting `acf`'s
/// rights. libca keeps the permits on an unresponsive circuit (only
/// `setServerAddressUnknown`, `nciu.cpp:184-195`, clears them, and
/// `tcpiiu::unresponsiveCircuitNotify` never calls it), so the rights
/// softIoc granted at connect are what the channel holds through the outage
/// and what the receiver is told after `Connected` on recovery.
async fn rights_survive_an_unresponsive_circuit(acf: &str) {
    use epics_ca_rs::client::ConnectionEvent;
    require_tool("softIoc");
    let ioc = spawn_softioc_with_acf("record(ao, \"RIGHTS:PV\") {}\n", acf);
    set_client_env(&ioc.ca_addr_list(), ioc.udp_port);
    // The echo watchdog arms after EPICS_CA_CONN_TMO of silence.
    unsafe { std::env::set_var("EPICS_CA_CONN_TMO", "1") };
    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CaClient");
    let (ch, mut rx) = client.create_channel_with_events("RIGHTS:PV", 0);
    // Every recv is bounded; the echo timeout is CONN_TMO + 5 s.
    let mut next = async || {
        tokio::time::timeout(fact_budget::FACT_BUDGET, rx.recv())
            .await
            .expect("connection event inside the budget")
            .expect("receiver open, not lagged")
    };
    let granted = match next().await {
        ConnectionEvent::AccessRightsChanged { read, write } => (read, write),
        other => panic!("softIoc's rights precede Connected, got {other:?}"),
    };
    assert_eq!(next().await, ConnectionEvent::Connected);

    signal(ioc.pid(), "-STOP");
    let down = (next().await, next().await);
    let rights = async || {
        ch.info()
            .await
            .ok()
            .map(|i| (i.access_rights.read, i.access_rights.write))
    };
    let held = rights().await;
    signal(ioc.pid(), "-CONT");
    assert_eq!(
        down,
        (
            ConnectionEvent::Disconnected,
            ConnectionEvent::AccessRightsChanged {
                read: false,
                write: false
            }
        ),
        "C: disconnectNotify then accessRightsNotify(noRights), nciu.cpp:170-177"
    );
    assert_eq!(
        held,
        Some(granted),
        "ca_write_access answers the held permits while the circuit is unresponsive"
    );

    assert_eq!(
        next().await,
        ConnectionEvent::AccessRightsChanged {
            read: granted.0,
            write: granted.1
        },
        "recovery reports the rights softIoc granted, not the outage's no-rights"
    );
    assert_eq!(next().await, ConnectionEvent::Connected);
    assert_eq!(rights().await, Some(granted));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn a_write_right_held_across_an_unresponsive_circuit() {
    rights_survive_an_unresponsive_circuit("ASG(DEFAULT) { RULE(1, WRITE) }").await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "spawns external libca softIoc; run with --include-ignored"]
async fn a_read_only_right_held_across_an_unresponsive_circuit() {
    rights_survive_an_unresponsive_circuit("ASG(DEFAULT) { RULE(1, READ) }").await;
}
