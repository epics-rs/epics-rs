//! Integration tests: CaClient <-> CaServer over TCP.
//!
//! These tests verify the end-to-end path for get, get_with_metadata,
//! put, and monitor operations. They also exercise the error paths
//! fixed in the DBR_TIME/CTRL work (ECA error propagation, snapshot
//! None handling, etc.).
//!
//! Gated on the `ca-server-tls-test` feature: `run_tcp_listener`'s
//! `tls` / `cap_token_verifier` params are `#[cfg]`-gated on
//! `epics-ca-rs`'s `experimental-rust-tls` / `cap-tokens` features.
//! `ca-server-tls-test` forces both on, so whenever this file
//! compiles the function has its full 12-arg signature and the call
//! below matches deterministically — under any workspace feature
//! union (e.g. `epics-bridge-rs` pulling those `epics-ca-rs` features
//! in independently).
#![cfg(feature = "ca-server-tls-test")]

// RTEMS-EXEC-MODEL-ALLOW(10): not built by default - this file is behind the `ca-server-tls-test` feature.

use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snapshot::DbrClass;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use serial_test::serial;

/// Spin up an ephemeral-port server with the given PVs, return
/// a connected `CaClient` whose ADDR_LIST points at that server.
async fn setup(pvs: Vec<(&str, EpicsValue)>) -> CaResult<epics_ca_rs::client::CaClient> {
    let db = Arc::new(PvDatabase::new());
    for (name, val) in pvs {
        db.add_pv(name, val).await.unwrap();
    }

    let acf = epics_base_rs::server::access_security::new_acf_cell(None);

    // Bind TCP on port 0 — the listener is accepting from here on, and
    // the chosen port is known without a start-up handshake.
    let bound_tcp = epics_ca_rs::server::tcp::bind_tcp_listeners(0)
        .await
        .expect("TCP listener bound");
    let tcp_port = bound_tcp.port();
    let db_tcp = db.clone();
    let acf_clone = acf.clone();
    tokio::spawn(async move {
        let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (acf_reload_tx, _) = tokio::sync::broadcast::channel::<()>(16);
        let _ = epics_ca_rs::server::tcp::run_tcp_listener(
            db_tcp,
            bound_tcp,
            acf_clone,
            acf_reload_tx,
            None, // conn_events: not subscribed in this test
            None, // audit
            drain,
            None, // stats: not asserted in this test
            None, // tls: not exercised in this test
            None, // cap_token_verifier: not exercised in this test
        )
        .await;
    });

    // Bind the UDP search responder on a kernel-assigned port and read the
    // port back — never probe a port and re-bind the number, which races
    // any other socket in this process. Bound here, so a SEARCH sent
    // immediately below is queued by the kernel rather than lost — no
    // sleep needed.
    let responders = epics_ca_rs::server::udp::bind_udp_responders(
        0,
        vec![std::net::Ipv4Addr::UNSPECIFIED],
        &[],
    )
    .expect("UDP responder bound");
    let udp_port = responders.port();
    let db_udp = db.clone();
    tokio::spawn(async move {
        let _ = epics_ca_rs::server::udp::run_udp_search_responder(
            db_udp,
            responders,
            tcp_port,
            Vec::new(),
        )
        .await;
    });

    // Point the client at our server's UDP port.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{udp_port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }

    epics_ca_rs::client::CaClient::new().await
}

// ---------------------------------------------------------------------------
// Basic get / put
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_double() {
    let client = setup(vec![("TEST:VAL", EpicsValue::Double(42.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:VAL");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let (dbr_type, value) = ch.get().await.unwrap();
    assert_eq!(dbr_type, DbFieldType::Double);
    assert_eq!(value, EpicsValue::Double(42.0));
}

#[tokio::test]
#[serial]
async fn test_put_and_readback() {
    let client = setup(vec![("TEST:SP", EpicsValue::Double(0.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:SP");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    ch.put(&EpicsValue::Double(99.5)).await.unwrap();

    let (_, value) = ch.get().await.unwrap();
    assert_eq!(value, EpicsValue::Double(99.5));
}

// ---------------------------------------------------------------------------
// get_with_metadata -- DBR_TIME
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_with_metadata_time_double() {
    let client = setup(vec![("TEST:TEMP", EpicsValue::Double(25.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:TEMP");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let snap = ch.get_with_metadata(DbrClass::Time).await.unwrap();
    assert_eq!(snap.value, EpicsValue::Double(25.0));
    // Timestamp should be non-zero (server sets current time)
    assert!(snap.timestamp > std::time::SystemTime::UNIX_EPOCH);
}

#[tokio::test]
#[serial]
async fn test_get_with_metadata_time_short() {
    let client = setup(vec![("TEST:INT", EpicsValue::Short(7))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:INT");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let snap = ch.get_with_metadata(DbrClass::Time).await.unwrap();
    assert_eq!(snap.value, EpicsValue::Short(7));
}

#[tokio::test]
#[serial]
async fn test_get_with_metadata_time_string() {
    let client = setup(vec![("TEST:STR", EpicsValue::String("hello".into()))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:STR");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let snap = ch.get_with_metadata(DbrClass::Time).await.unwrap();
    assert_eq!(snap.value, EpicsValue::String("hello".into()));
}

// ---------------------------------------------------------------------------
// get_with_metadata -- DBR_CTRL
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_with_metadata_ctrl_double() {
    let client = setup(vec![("TEST:CTRL", EpicsValue::Double(PI))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:CTRL");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let snap = ch.get_with_metadata(DbrClass::Ctrl).await.unwrap();
    assert_eq!(snap.value, EpicsValue::Double(PI));
    // SimplePv has no display metadata, so fields should be default/zero
    assert_eq!(snap.alarm.status, 0);
    assert_eq!(snap.alarm.severity, 0);
}

// ---------------------------------------------------------------------------
// get_with_metadata -- DBR_STS
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_with_metadata_sts() {
    let client = setup(vec![("TEST:STS", EpicsValue::Double(1.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:STS");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let snap = ch.get_with_metadata(DbrClass::Sts).await.unwrap();
    assert_eq!(snap.value, EpicsValue::Double(1.0));
    assert_eq!(snap.alarm.status, 0);
}

// ---------------------------------------------------------------------------
// Monitor / subscribe
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_monitor_receives_updates() {
    let client = setup(vec![("TEST:MON", EpicsValue::Double(0.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:MON");
    ch.wait_connected(Duration::from_secs(3)).await.unwrap();

    let mut monitor = ch.subscribe().await.unwrap();

    // First callback is the current value
    let snap = tokio::time::timeout(Duration::from_secs(3), monitor.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snap.value, EpicsValue::Double(0.0));

    // Put a new value and expect the monitor to fire
    ch.put(&EpicsValue::Double(123.0)).await.unwrap();

    let snap = tokio::time::timeout(Duration::from_secs(3), monitor.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snap.value, EpicsValue::Double(123.0));
}

// ---------------------------------------------------------------------------
// Error handling: channel not found
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_nonexistent_pv_times_out() {
    let client = setup(vec![("TEST:EXISTS", EpicsValue::Double(0.0))])
        .await
        .unwrap();

    let ch = client.create_channel("TEST:DOES_NOT_EXIST");
    let result = ch.wait_connected(Duration::from_secs(2)).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// All native types through get_with_metadata(Time)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_get_with_metadata_all_native_types() {
    let pvs = vec![
        ("T:DOUBLE", EpicsValue::Double(1.5)),
        ("T:FLOAT", EpicsValue::Float(2.5)),
        ("T:LONG", EpicsValue::Long(42)),
        ("T:SHORT", EpicsValue::Short(-7)),
        ("T:CHAR", EpicsValue::Char(0xAB)),
        ("T:ENUM", EpicsValue::Enum(3)),
        ("T:STRING", EpicsValue::String("test".into())),
    ];
    let client = setup(pvs).await.unwrap();

    for (name, expected) in [
        ("T:DOUBLE", EpicsValue::Double(1.5)),
        ("T:FLOAT", EpicsValue::Float(2.5)),
        ("T:LONG", EpicsValue::Long(42)),
        ("T:SHORT", EpicsValue::Short(-7)),
        ("T:CHAR", EpicsValue::Char(0xAB)),
        ("T:ENUM", EpicsValue::Enum(3)),
        ("T:STRING", EpicsValue::String("test".into())),
    ] {
        let ch = client.create_channel(name);
        ch.wait_connected(Duration::from_secs(3)).await.unwrap();

        let snap = ch.get_with_metadata(DbrClass::Time).await.unwrap();
        assert_eq!(snap.value, expected, "mismatch for {name}");
    }
}
