//! CA-FR-4 regression tests: `caget -d <type>` must request the EXACT
//! DBR type code, not collapse it to a metadata class and re-derive the
//! value type from the channel's native type.
//!
//! C `caget` keeps the requested `dbrType` verbatim (`caget.c:172`,
//! `format == specifiedDbr`): `-d DBR_TIME_FLOAT` on a DOUBLE PV asks
//! the server for `DBR_TIME_FLOAT` (16) and receives a converted float,
//! and `-d 38`/`DBR_CLASS_NAME` reaches the record-class introspection
//! type. Pre-fix `caget-rs` mapped the token to a `DbrClass` band, so
//! the request type was re-derived as `DBR_TIME_DOUBLE` (20) and the
//! `37`/`38` codes mis-routed to a value class.
//!
//! These drive a real `CaClient` ↔ `CaServer` TCP round-trip through the
//! new `CaChannel::get_with_dbr_type`, which is the exact wire request
//! that the `caget -d` front-end issues.

use std::time::Duration;

use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{
    DBR_CHAR, DBR_CLASS_NAME, DBR_TIME_DOUBLE, DBR_TIME_FLOAT, DbFieldType,
};
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Reserve and immediately release a free localhost TCP port so the
/// `CaServer` can bind it.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
    let p = probe.local_addr().unwrap().port();
    drop(probe);
    p
}

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

/// Bring up a server holding one DOUBLE waveform seeded with the given
/// ramp, returning a connected client channel ready to read.
async fn server_with_double_waveform(
    pv: &'static str,
    seed: Vec<f64>,
) -> (CaClient, epics_ca_rs::client::CaChannel) {
    let port = free_port();
    let len = seed.len() as i32;
    let server = CaServer::builder()
        .port(port)
        .record(pv, WaveformRecord::new(len, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let _h = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(pv);
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    ch.put(&EpicsValue::DoubleArray(seed))
        .await
        .expect("seed waveform");
    (client, ch)
}

/// CA-FR-4 (1/2): a DOUBLE PV read with `-d DBR_TIME_FLOAT` returns a
/// FLOAT value, proving the exact requested code is honoured rather
/// than re-derived to the native `DBR_TIME_DOUBLE`. The companion
/// `DBR_TIME_DOUBLE` request returns DOUBLE, so the two codes are not
/// interchangeable — the type travels to the wire verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn caget_dbr_type_honors_exact_value_type() {
    // 1.5/2.5/3.5 are exactly representable in both f32 and f64, so the
    // FLOAT round-trip is bit-exact and the assertion is not lossy.
    let (_client, ch) = server_with_double_waveform("CAFR4:WF:FLT", vec![1.5, 2.5, 3.5]).await;

    let as_float = ch
        .get_with_dbr_type(DBR_TIME_FLOAT, 0)
        .await
        .expect("DBR_TIME_FLOAT get");
    match &as_float.value {
        EpicsValue::FloatArray(a) => assert_eq!(a.as_slice(), &[1.5_f32, 2.5, 3.5]),
        other => panic!("-d DBR_TIME_FLOAT must yield FloatArray, got {other:?}"),
    }

    let as_double = ch
        .get_with_dbr_type(DBR_TIME_DOUBLE, 0)
        .await
        .expect("DBR_TIME_DOUBLE get");
    match &as_double.value {
        EpicsValue::DoubleArray(a) => assert_eq!(a.as_slice(), &[1.5_f64, 2.5, 3.5]),
        other => panic!("-d DBR_TIME_DOUBLE must yield DoubleArray, got {other:?}"),
    }
}

/// CA-FR-4 (2/2): `-d DBR_CLASS_NAME` (38) reaches the record-class
/// introspection type and returns the record's type name. Pre-fix the
/// `38` code fell into the `_ => Plain` band and never reached the
/// server's `DBR_CLASS_NAME` handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn caget_dbr_type_reaches_class_name() {
    let (_client, ch) = server_with_double_waveform("CAFR4:WF:CLS", vec![0.0, 0.0]).await;

    let snap = ch
        .get_with_dbr_type(DBR_CLASS_NAME, 0)
        .await
        .expect("DBR_CLASS_NAME get");
    assert_eq!(
        snap.class_name.as_deref(),
        Some("waveform"),
        "DBR_CLASS_NAME must carry the record's type name"
    );
}

/// R55 boundary 1: autosize GET on a `$`-suffix channel returns exactly
/// `MAX_STRING_SIZE` (= 40) bytes — string, NUL terminator, zero-padded.
/// C `dbChannel.c:489` sets `no_elements = field_size` (= 40); the Rust
/// server must advertise 40 on CREATE_CHAN and deliver 40 on every read.
/// Pre-fix `apply_long_string` emitted `strlen+1` bytes; autosize
/// `caget PV.VAL$` returned 6 bytes instead of 40 for "hello".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn long_string_autosize_returns_40_nul_padded() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .record("R55:LS:AUTO", StringinRecord::new("hello"))
        .build()
        .await
        .expect("build CA server");
    let _h = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    // The `$` suffix requests the VAL field as a DBR_CHAR array.
    let ch = client.create_channel("R55:LS:AUTO.VAL$");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to long-string channel");

    // Autosize (count=0) must return exactly 40 elements (field_size).
    let snap = ch
        .get_with_dbr_type(DBR_CHAR, 0)
        .await
        .expect("autosize DBR_CHAR get");
    let bytes = match snap.value {
        EpicsValue::CharArray(ref b) => b.as_slice(),
        ref other => panic!("expected CharArray, got {other:?}"),
    };
    assert_eq!(
        bytes.len(),
        40,
        "autosize `$` channel must return 40 bytes (= MAX_STRING_SIZE); got {}",
        bytes.len()
    );
    // First 5 bytes are "hello", byte 5 is NUL, bytes 6-39 are zero.
    assert_eq!(&bytes[..5], b"hello", "string content must match");
    assert_eq!(bytes[5], 0, "byte after string must be NUL terminator");
    assert!(
        bytes[6..].iter().all(|&b| b == 0),
        "trailing bytes must be zero-padded"
    );
}

/// R55 boundary 2: a count-clamped GET on a `$` channel trims the 40-byte
/// array to the requested count (C `read_reply` dbr_size_n parity).
/// Pre-fix the clamp ran BEFORE apply_long_string so it saw
/// `EpicsValue::String::count() == 1` and the predicate `count < 1`
/// was always false — `caget -# 3 PV.VAL$` returned 40 bytes, not 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn long_string_count_clamp_trims_char_array() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .record("R55:LS:CLAMP", StringinRecord::new("hello"))
        .build()
        .await
        .expect("build CA server");
    let _h = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("R55:LS:CLAMP.VAL$");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to long-string channel");

    // count=3 must trim the 40-element char array to 3 bytes "hel".
    let snap = ch
        .get_with_dbr_type(DBR_CHAR, 3)
        .await
        .expect("count-3 DBR_CHAR get");
    let bytes = match snap.value {
        EpicsValue::CharArray(ref b) => b.as_slice(),
        ref other => panic!("expected CharArray, got {other:?}"),
    };
    assert_eq!(
        bytes.len(),
        3,
        "`caget -# 3 PV.VAL$` must return 3 bytes; got {}",
        bytes.len()
    );
    assert_eq!(
        bytes, b"hel",
        "trimmed content must be first 3 chars of string"
    );
}
