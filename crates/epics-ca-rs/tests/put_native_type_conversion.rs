//! Regression tests: the native-typed put family (`put`,
//! `put_with_timeout`, `put_nowait`) must CONVERT the caller's value to
//! the channel's native type before encoding the payload.
//!
//! Those functions stamp `snap.native_type` on the frame header, so the
//! payload bytes must be the native encoding. Pre-fix they encoded the
//! caller's variant verbatim: `put(&EpicsValue::Long(1))` on an ENUM
//! field (`bi`) shipped a DBR_ENUM header over a 4-byte big-endian i32,
//! and the server — decoding strictly against the header type — read
//! the first two bytes (`0x0000`) as the enum index. `Long(1)` landed
//! as 0 with a successful completion callback; every write of 0 "worked"
//! by accident, which is what hid the defect.
//!
//! C has no such seam: `ca_array_put(type, ...)` takes the caller's
//! DBR type and buffer together, and the SERVER converts to the field
//! type. The Rust native-typed API's equivalent obligation is client-
//! side conversion through `convert_to`, the value-coercion owner.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::time::{Duration, Instant};

use epics_base_rs::server::records::bi::BiRecord;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point a soon-to-be-constructed `CaClient` at exactly this server so
/// it skips UDP search.
///
/// SAFETY: every test in this file is `#[serial]`, so no other test
/// mutates the environment concurrently, and the env is set before
/// `CaClient::new()` snapshots its resolver configuration.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// Bring up a server holding one `bi` record (native ENUM, VAL=0),
/// returning a connected client channel.
async fn server_with_bi(pv: &'static str) -> (CaClient, epics_ca_rs::client::CaChannel) {
    let mut rec = BiRecord::new(0);
    rec.znam = "Off".into();
    rec.onam = "On".into();
    let server = CaServer::builder()
        .port(0)
        .record(pv, rec)
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(pv);
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect");
    (client, ch)
}

/// Native readback as a plain integer, whatever ENUM carrier comes back.
async fn read_index(ch: &epics_ca_rs::client::CaChannel) -> i64 {
    let (_t, v) = ch.get().await.expect("readback");
    match v {
        EpicsValue::Enum(i) => i as i64,
        EpicsValue::EnumWithChoices { index, .. } => index as i64,
        other => panic!("unexpected readback variant: {other:?}"),
    }
}

/// Owner path: a variant already matching the native type is encoded
/// verbatim — conversion is the identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn matching_enum_put_lands_index() {
    let (_client, ch) = server_with_bi("PNC:BI:ENUM").await;
    ch.put(&EpicsValue::Enum(1)).await.expect("put");
    assert_eq!(read_index(&ch).await, 1);
}

/// Formerly-corrupted path: `Long(1)` on the ENUM-native `bi` must land
/// as index 1. Pre-fix the DBR_ENUM header over Long bytes decoded to 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn long_put_on_enum_native_converts() {
    let (_client, ch) = server_with_bi("PNC:BI:LONG").await;
    ch.put_with_timeout(&EpicsValue::Long(1), budget::FACT_BUDGET)
        .await
        .expect("put");
    assert_eq!(read_index(&ch).await, 1);
}

/// Float source takes the `putDoubleEnum` wrapping-cast lane of
/// `convert_to` rather than the integer view — same header/payload
/// invariant, different coercion path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn double_put_on_enum_native_converts() {
    let (_client, ch) = server_with_bi("PNC:BI:DBL").await;
    ch.put(&EpicsValue::Double(1.0)).await.expect("put");
    assert_eq!(read_index(&ch).await, 1);
}

/// Fire-and-forget lane: `put_nowait` shares the encoding seam but has
/// no completion callback, so poll the readback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn nowait_long_put_on_enum_native_converts() {
    let (_client, ch) = server_with_bi("PNC:BI:NOWAIT").await;
    ch.put_nowait(&EpicsValue::Long(1)).await.expect("put");

    let deadline = Instant::now() + budget::FACT_BUDGET;
    loop {
        if read_index(&ch).await == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "put_nowait value never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[path = "common/budget.rs"]
mod budget;
