//! Integration tests for epics-ca-rs: protocol encoding/decoding and server API.

// Used only by the async `CaServer` tests below, which are gated off under
// `rtems-exec-model`; the pure wire-format tests need only `protocol::*`.
#[cfg(not(feature = "rtems-exec-model"))]
use std::collections::HashMap;

#[cfg(not(feature = "rtems-exec-model"))]
use epics_ca_rs::EpicsValue;
use epics_ca_rs::protocol::*;
#[cfg(not(feature = "rtems-exec-model"))]
use epics_ca_rs::server::CaServer;
#[cfg(not(feature = "rtems-exec-model"))]
use serial_test::serial;

// ---------------------------------------------------------------------------
// CA protocol header encoding/decoding
// ---------------------------------------------------------------------------

#[test]
fn header_roundtrip_all_commands() {
    let commands = [
        CA_PROTO_VERSION,
        CA_PROTO_EVENT_ADD,
        CA_PROTO_EVENT_CANCEL,
        CA_PROTO_SEARCH,
        CA_PROTO_NOT_FOUND,
        CA_PROTO_READ_NOTIFY,
        CA_PROTO_CREATE_CHAN,
        CA_PROTO_WRITE_NOTIFY,
        CA_PROTO_HOST_NAME,
        CA_PROTO_CLIENT_NAME,
        CA_PROTO_ACCESS_RIGHTS,
        CA_PROTO_ECHO,
        CA_PROTO_REPEATER_CONFIRM,
        CA_PROTO_REPEATER_REGISTER,
        CA_PROTO_CLEAR_CHANNEL,
        CA_PROTO_RSRV_IS_UP,
        CA_PROTO_SERVER_DISCONN,
        CA_PROTO_READ,
        CA_PROTO_WRITE,
        CA_PROTO_EVENTS_OFF,
        CA_PROTO_EVENTS_ON,
        CA_PROTO_READ_SYNC,
        CA_PROTO_ERROR,
        CA_PROTO_CREATE_CH_FAIL,
    ];
    for cmmd in commands {
        let hdr = CaHeader {
            cmmd,
            postsize: 32,
            data_type: 6,
            count: 1,
            cid: 0xDEAD,
            available: 0xBEEF,
            extended_postsize: None,
            extended_count: None,
        };
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), CaHeader::SIZE);
        let hdr2 = CaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr.cmmd, hdr2.cmmd, "command mismatch for cmmd={cmmd}");
        assert_eq!(hdr.postsize, hdr2.postsize);
        assert_eq!(hdr.data_type, hdr2.data_type);
        assert_eq!(hdr.count, hdr2.count);
        assert_eq!(hdr.cid, hdr2.cid);
        assert_eq!(hdr.available, hdr2.available);
    }
}

#[test]
fn header_extended_roundtrip_via_to_bytes_extended() {
    let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
    hdr.data_type = 6;
    hdr.cid = 999;
    hdr.available = 888;
    hdr.set_payload_size(200_000, 25_000, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");

    assert!(hdr.is_extended());
    let bytes = hdr.to_bytes_extended();
    assert_eq!(bytes.len(), 24);

    let (decoded, consumed) = CaHeader::from_bytes_extended(&bytes).unwrap();
    assert_eq!(consumed, 24);
    assert!(decoded.is_extended());
    assert_eq!(decoded.actual_postsize(), 200_000);
    assert_eq!(decoded.actual_count(), 25_000);
    assert_eq!(decoded.cmmd, CA_PROTO_EVENT_ADD);
    assert_eq!(decoded.data_type, 6);
    assert_eq!(decoded.cid, 999);
    assert_eq!(decoded.available, 888);
}

#[test]
fn header_normal_stays_normal_when_small() {
    let mut hdr = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    hdr.set_payload_size(500, 10, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(!hdr.is_extended());
    assert_eq!(hdr.postsize, 500);
    assert_eq!(hdr.count, 10);
    assert_eq!(hdr.actual_postsize(), 500);
    assert_eq!(hdr.actual_count(), 10);
}

#[test]
fn header_from_bytes_too_short() {
    let short_buf = [0u8; 10];
    assert!(CaHeader::from_bytes(&short_buf).is_err());
}

#[test]
fn header_extended_from_bytes_incomplete() {
    // Build a header that claims extended (postsize=0xFFFF, count=0),
    // but only supply 16 bytes, not the required 24.
    let mut buf = [0u8; 16];
    buf[2] = 0xFF;
    buf[3] = 0xFF;
    // count = 0 (already zero)
    let result = CaHeader::from_bytes_extended(&buf);
    assert!(result.is_err());
}

#[test]
fn pad_string_various_lengths() {
    // Empty string: "\0" = 1 byte -> align8 = 8
    let p = pad_string("");
    assert_eq!(p.len(), 8);
    assert_eq!(p[0], 0);

    // Exactly 7 chars: "ABCDEFG\0" = 8 -> align8 = 8
    let p = pad_string("ABCDEFG");
    assert_eq!(p.len(), 8);
    assert_eq!(&p[..7], b"ABCDEFG");
    assert_eq!(p[7], 0);

    // 8 chars: "ABCDEFGH\0" = 9 -> align8 = 16
    let p = pad_string("ABCDEFGH");
    assert_eq!(p.len(), 16);
    assert_eq!(&p[..8], b"ABCDEFGH");
    assert_eq!(p[8], 0);
}

#[test]
fn defmsg_encoding() {
    // ECA_NORMAL should be 1
    assert_eq!(ECA_NORMAL, 1);
    // Check a known value: ECA_BADTYPE = defmsg(2, 14) = (14 << 3 & 0xFFF8) | (2 & 7) = 112 | 2 = 114
    assert_eq!(ECA_BADTYPE, 114);
    // ECA_PUTFAIL = defmsg(0, 20) = (20 << 3 & 0xFFF8) | 0 = 160
    assert_eq!(ECA_PUTFAIL, 160);
}

#[test]
fn align8_boundary_values() {
    assert_eq!(align8(0), 0);
    assert_eq!(align8(1), 8);
    assert_eq!(align8(8), 8);
    assert_eq!(align8(16), 16);
    assert_eq!(align8(17), 24);
    assert_eq!(align8(100), 104);
}

#[test]
fn header_set_payload_boundary_at_0xfffe() {
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);

    // 0xFFFE should still fit in normal form
    hdr.set_payload_size(0xFFFE, 1, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(!hdr.is_extended());
    assert_eq!(hdr.postsize, 0xFFFE);

    // 0xFFFF triggers extended
    hdr.set_payload_size(0xFFFF, 1, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_postsize(), 0xFFFF);
}

#[test]
fn header_set_payload_count_boundary_at_0xffff() {
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);

    // count = 0xFFFE fits in normal form
    hdr.set_payload_size(100, 0xFFFE, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(!hdr.is_extended());
    assert_eq!(hdr.count, 0xFFFE);

    // count = 0xFFFF triggers extended (C `comQueSend.cpp:285` —
    // `nElem < 0xffff` is the normal threshold, so exact `0xFFFF`
    // requires extended form).
    hdr.set_payload_size(100, 0xFFFF, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_count(), 0xFFFF);

    // count = 0x10000 triggers extended
    hdr.set_payload_size(100, 0x10000, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_count(), 0x10000);
}

// ---------------------------------------------------------------------------
// CaServer builder pattern — basic construction with simple PVs
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_builder_with_simple_pvs() {
    let server = CaServer::builder()
        .port(0)
        .pv("TEST:DOUBLE", EpicsValue::Double(3.15))
        .pv("TEST:STRING", EpicsValue::String("hello".into()))
        .pv("TEST:SHORT", EpicsValue::Short(42))
        .pv("TEST:ENUM", EpicsValue::Enum(2))
        .build()
        .await
        .unwrap();

    // Verify get returns the initial values
    assert_eq!(
        server.get("TEST:DOUBLE").await.unwrap(),
        EpicsValue::Double(3.15)
    );
    assert_eq!(
        server.get("TEST:STRING").await.unwrap(),
        EpicsValue::String("hello".into())
    );
    assert_eq!(
        server.get("TEST:SHORT").await.unwrap(),
        EpicsValue::Short(42)
    );
    assert_eq!(server.get("TEST:ENUM").await.unwrap(), EpicsValue::Enum(2));
}

// ---------------------------------------------------------------------------
// CaServer get/put with different value types
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_double() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:D", EpicsValue::Double(0.0))
        .build()
        .await
        .unwrap();

    server.put("SRV:D", EpicsValue::Double(99.9)).await.unwrap();
    assert_eq!(server.get("SRV:D").await.unwrap(), EpicsValue::Double(99.9));
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_string() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:S", EpicsValue::String("initial".into()))
        .build()
        .await
        .unwrap();

    server
        .put("SRV:S", EpicsValue::String("updated".into()))
        .await
        .unwrap();
    assert_eq!(
        server.get("SRV:S").await.unwrap(),
        EpicsValue::String("updated".into())
    );
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_short() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:I", EpicsValue::Short(0))
        .build()
        .await
        .unwrap();

    server.put("SRV:I", EpicsValue::Short(-123)).await.unwrap();
    assert_eq!(server.get("SRV:I").await.unwrap(), EpicsValue::Short(-123));
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_enum() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:E", EpicsValue::Enum(0))
        .build()
        .await
        .unwrap();

    server.put("SRV:E", EpicsValue::Enum(5)).await.unwrap();
    assert_eq!(server.get("SRV:E").await.unwrap(), EpicsValue::Enum(5));
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_float() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:F", EpicsValue::Float(0.0))
        .build()
        .await
        .unwrap();

    server.put("SRV:F", EpicsValue::Float(2.5)).await.unwrap();
    assert_eq!(server.get("SRV:F").await.unwrap(), EpicsValue::Float(2.5));
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_long() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:L", EpicsValue::Long(0))
        .build()
        .await
        .unwrap();

    server
        .put("SRV:L", EpicsValue::Long(1_000_000))
        .await
        .unwrap();
    assert_eq!(
        server.get("SRV:L").await.unwrap(),
        EpicsValue::Long(1_000_000)
    );
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_and_get_char() {
    let server = CaServer::builder()
        .port(0)
        .pv("SRV:C", EpicsValue::Char(0))
        .build()
        .await
        .unwrap();

    server.put("SRV:C", EpicsValue::Char(0xAB)).await.unwrap();
    assert_eq!(server.get("SRV:C").await.unwrap(), EpicsValue::Char(0xAB));
}

// ---------------------------------------------------------------------------
// CaServer get nonexistent PV returns error
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_get_nonexistent_pv_returns_error() {
    let server = CaServer::builder()
        .port(0)
        .pv("REAL:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .unwrap();

    let result = server.get("DOES:NOT:EXIST").await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// PR #592 dbServerStats — bytes_in / bytes_out counters
// ---------------------------------------------------------------------------

/// A real TCP round-trip (CaClient ↔ CaServer) must increment both
/// `bytes_in` and `bytes_out` on the server's `ServerStats`. Pre-fix
/// these counters were declared and exposed via `casr` but never
/// updated by the read/flush hot path — operators saw `bytes in=0,
/// out=0` no matter how much traffic flowed.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_stats_bytes_in_out_track_real_traffic() {
    use std::time::Duration;

    let server = CaServer::builder()
        .port(0)
        .pv("STATS:BYTES", EpicsValue::Double(7.5))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let stats = server.stats();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    // Drive a real CA caget through the local TCP listener. Tell the
    // client exactly where to find this server so it skips UDP search.
    // SAFETY: tokio test runtime is multi-threaded; we're mutating env
    // before the client constructs its name-resolver state.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let client = epics_ca_rs::client::CaClient::new().await.expect("client");
    let (_ty, val) = tokio::time::timeout(Duration::from_secs(5), client.caget("STATS:BYTES"))
        .await
        .expect("caget did not complete within 5s")
        .expect("caget should succeed against local server");
    match val {
        EpicsValue::Double(d) => assert!((d - 7.5).abs() < 1e-10, "round-trip value {d}"),
        other => panic!("expected Double(7.5), got {other:?}"),
    }

    use std::sync::atomic::Ordering::Relaxed;
    let bin = stats.bytes_in.load(Relaxed);
    let bout = stats.bytes_out.load(Relaxed);
    assert!(
        bin > 0,
        "bytes_in must increment for a real CA round-trip; got 0"
    );
    assert!(
        bout > 0,
        "bytes_out must increment for a real CA round-trip; got 0"
    );
    // Sanity: the response is normally larger than the request once
    // CTRL/STS metadata is included. We don't pin an exact ratio
    // (depends on DBR type encoding), just that both are nontrivial.
    assert!(
        bin >= 16,
        "bytes_in {bin} too small — should at least cover CA header(s)"
    );
    assert!(
        bout >= 16,
        "bytes_out {bout} too small — should at least cover CA header(s)"
    );
}

/// PR #592 follow-up: subscription counters track EVENT_ADD opens
/// and EVENT_CANCEL/teardown closes. Pre-wiring the counters were
/// declared and printed by `casr` but never incremented. Asserts
/// that `subscriptions_opened_total` and `subscriptions_closed_total`
/// both increment for a normal monitor lifecycle.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_stats_subscription_counters_track_camonitor_lifecycle() {
    use std::time::Duration;

    let server = CaServer::builder()
        .port(0)
        .pv("STATS:SUB:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let stats = server.stats();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let client = epics_ca_rs::client::CaClient::new().await.expect("client");
    let channel = client.create_channel("STATS:SUB:PV");
    let mut monitor = channel.subscribe().await.expect("subscribe");
    // Wait for the initial monitor frame so we know EVENT_ADD has
    // been accepted server-side.
    let _initial = tokio::time::timeout(Duration::from_secs(2), monitor.recv())
        .await
        .expect("initial monitor frame did not arrive")
        .expect("monitor stream yielded no value");

    use std::sync::atomic::Ordering::Relaxed;
    let opened = stats.subscriptions_opened_total.load(Relaxed);
    assert!(
        opened >= 1,
        "subscriptions_opened_total must increment after EVENT_ADD; got {opened}"
    );

    // Drop the monitor — server-side teardown path runs either via
    // EVENT_CANCEL (if the client sends it) or via the
    // ChannelCleared → subscription drain path. Both increment the
    // closed counter.
    drop(monitor);
    drop(channel);
    drop(client);

    // Give the server a moment to process the disconnect / clear.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let closed = stats.subscriptions_closed_total.load(Relaxed);
    assert!(
        closed >= 1,
        "subscriptions_closed_total must increment after teardown; got {closed}"
    );
    assert_eq!(
        opened, closed,
        "open and close counts must match for a clean lifecycle: {opened} vs {closed}"
    );
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_nonexistent_pv_returns_error() {
    let server = CaServer::builder()
        .port(0)
        .pv("REAL:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .unwrap();

    let result = server.put("DOES:NOT:EXIST", EpicsValue::Double(1.0)).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// CaServer add_pv at runtime
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_add_pv_at_runtime() {
    let server = CaServer::builder().port(0).build().await.unwrap();

    // PV does not exist yet
    assert!(server.get("RUNTIME:PV").await.is_err());

    // Add it
    server
        .add_pv("RUNTIME:PV", EpicsValue::Double(42.0))
        .await
        .unwrap();

    // Now it exists
    assert_eq!(
        server.get("RUNTIME:PV").await.unwrap(),
        EpicsValue::Double(42.0)
    );
}

// ---------------------------------------------------------------------------
// CaServer with multiple PVs of different types
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_multiple_pv_types_coexist() {
    let server = CaServer::builder()
        .port(0)
        .pv("MULTI:D", EpicsValue::Double(1.1))
        .pv("MULTI:S", EpicsValue::String("abc".into()))
        .pv("MULTI:I", EpicsValue::Short(7))
        .pv("MULTI:E", EpicsValue::Enum(1))
        .pv("MULTI:F", EpicsValue::Float(3.0))
        .pv("MULTI:L", EpicsValue::Long(-100))
        .pv("MULTI:C", EpicsValue::Char(65))
        .build()
        .await
        .unwrap();

    assert_eq!(
        server.get("MULTI:D").await.unwrap(),
        EpicsValue::Double(1.1)
    );
    assert_eq!(
        server.get("MULTI:S").await.unwrap(),
        EpicsValue::String("abc".into())
    );
    assert_eq!(server.get("MULTI:I").await.unwrap(), EpicsValue::Short(7));
    assert_eq!(server.get("MULTI:E").await.unwrap(), EpicsValue::Enum(1));
    assert_eq!(server.get("MULTI:F").await.unwrap(), EpicsValue::Float(3.0));
    assert_eq!(server.get("MULTI:L").await.unwrap(), EpicsValue::Long(-100));
    assert_eq!(server.get("MULTI:C").await.unwrap(), EpicsValue::Char(65));
}

// ---------------------------------------------------------------------------
// CaServer with db_string — load records from EPICS .db text
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_builder_db_string_ai_record() {
    let db_text = r#"
record(ai, "TEMP:READING") {
    field(VAL, "25.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .port(0)
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let val = server.get("TEMP:READING").await.unwrap();
    assert_eq!(val, EpicsValue::Double(25.0));
}

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_builder_db_string_with_macros() {
    let db_text = r#"
record(ai, "$(PREFIX):SETPOINT") {
    field(VAL, "100.0")
}
"#;
    let mut macros = HashMap::new();
    macros.insert("PREFIX".to_string(), "MTR01".to_string());
    let server = CaServer::builder()
        .port(0)
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let val = server.get("MTR01:SETPOINT").await.unwrap();
    assert_eq!(val, EpicsValue::Double(100.0));
}

// ---------------------------------------------------------------------------
// Record field access via "PV.FIELD" syntax
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_record_field_access_dot_syntax() {
    let db_text = r#"
record(ai, "SENSOR:TEMP") {
    field(VAL, "20.0")
    field(EGU, "degC")
    field(DESC, "Temperature sensor")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .port(0)
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    // Bare name defaults to .VAL
    let val = server.get("SENSOR:TEMP").await.unwrap();
    assert_eq!(val, EpicsValue::Double(20.0));

    // Explicit .VAL
    let val = server.get("SENSOR:TEMP.VAL").await.unwrap();
    assert_eq!(val, EpicsValue::Double(20.0));

    // .EGU field
    let egu = server.get("SENSOR:TEMP.EGU").await.unwrap();
    assert_eq!(egu, EpicsValue::String("degC".into()));

    // .DESC field
    let desc = server.get("SENSOR:TEMP.DESC").await.unwrap();
    assert_eq!(desc, EpicsValue::String("Temperature sensor".into()));
}

// ---------------------------------------------------------------------------
// Server with multiple record types
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_multiple_record_types() {
    let db_text = r#"
record(ai, "AI:VAL") {
    field(VAL, "1.5")
}
record(ao, "AO:VAL") {
    field(VAL, "2.5")
}
record(bi, "BI:VAL") {
    field(VAL, "1")
}
record(bo, "BO:VAL") {
    field(VAL, "0")
}
record(longin, "LI:VAL") {
    field(VAL, "42")
}
record(longout, "LO:VAL") {
    field(VAL, "99")
}
record(stringin, "SI:VAL") {
    field(VAL, "hello")
}
record(stringout, "SO:VAL") {
    field(VAL, "world")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .port(0)
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(server.get("AI:VAL").await.unwrap(), EpicsValue::Double(1.5));
    assert_eq!(server.get("AO:VAL").await.unwrap(), EpicsValue::Double(2.5));
    assert_eq!(server.get("LI:VAL").await.unwrap(), EpicsValue::Long(42));
    assert_eq!(server.get("LO:VAL").await.unwrap(), EpicsValue::Long(99));
    assert_eq!(
        server.get("SI:VAL").await.unwrap(),
        EpicsValue::String("hello".into())
    );
    assert_eq!(
        server.get("SO:VAL").await.unwrap(),
        EpicsValue::String("world".into())
    );
}

// ---------------------------------------------------------------------------
// Put to a record field via CaServer::put
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_put_to_record() {
    let db_text = r#"
record(ao, "CTRL:SP") {
    field(VAL, "0.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .port(0)
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    // Initial value
    assert_eq!(
        server.get("CTRL:SP").await.unwrap(),
        EpicsValue::Double(0.0)
    );

    // Put a new value
    server
        .put("CTRL:SP", EpicsValue::Double(50.0))
        .await
        .unwrap();
    assert_eq!(
        server.get("CTRL:SP").await.unwrap(),
        EpicsValue::Double(50.0)
    );
}

// ---------------------------------------------------------------------------
// Mixed: builder PVs + db_string records
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_mixed_simple_pvs_and_records() {
    let db_text = r#"
record(ai, "REC:AI") {
    field(VAL, "10.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .port(0)
        .pv("SIMPLE:PV", EpicsValue::Double(20.0))
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(
        server.get("SIMPLE:PV").await.unwrap(),
        EpicsValue::Double(20.0)
    );
    assert_eq!(
        server.get("REC:AI").await.unwrap(),
        EpicsValue::Double(10.0)
    );
}

// ---------------------------------------------------------------------------
// Server builder with custom port
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_builder_custom_port() {
    // This just verifies the builder accepts port() without error.
    // We don't actually start the network stack in these tests.
    let server = CaServer::builder()
        .port(9999)
        .pv("PORT:TEST", EpicsValue::Double(1.0))
        .build()
        .await
        .unwrap();

    assert_eq!(
        server.get("PORT:TEST").await.unwrap(),
        EpicsValue::Double(1.0)
    );
}

// ---------------------------------------------------------------------------
// Server database() accessor
// ---------------------------------------------------------------------------

// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn server_database_accessor() {
    let server = CaServer::builder()
        .port(0)
        .pv("DB:ACCESS", EpicsValue::Double(7.7))
        .build()
        .await
        .unwrap();

    // Access the underlying PvDatabase and verify it can find the PV
    let db = server.database();
    assert!(db.has_name("DB:ACCESS").await);
    assert!(!db.has_name("NONEXISTENT").await);
}

/// C `tcp_echo_action` (`rsrv/camessage.c:403-420`) echoes the full
/// request header AND payload back to the client. The previous Rust
/// behaviour replied with an all-zero CA_PROTO_ECHO header, dropping
/// the request fields and any payload. Real clients (libca
/// `tcpiiu::echoRequest`) only ever send zero-payload echos so the
/// difference was masked in practice, but a diagnostic / probe client
/// that puts a marker payload (e.g. RTT measurement, transparent-
/// proxy detection) saw a stripped reply — a wire-level divergence.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_echo_round_trips_request_header_and_payload() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("ECHO:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // Handshake: send VERSION, drain BOTH server VERSION frames
    // (unsolicited VERSION on accept + VERSION reply = 32 bytes).
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    let mut drained = 0;
    while drained < 32 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[drained..]))
            .await
            .expect("server VERSION drain timed out")
            .expect("read VERSION");
        if n == 0 {
            break;
        }
        drained += n;
    }
    assert!(
        drained >= 32,
        "expected 2 VERSION frames; got {drained} bytes"
    );

    // Send CA_PROTO_ECHO with a non-trivial header AND an 8-byte
    // payload — the C server is documented to echo m_postsize bytes
    // verbatim.
    let mut echo = CaHeader::new(CA_PROTO_ECHO);
    echo.data_type = 0xAAAA;
    echo.count = 0; // padded post-write — set_payload_size below will adjust
    echo.cid = 0x1122_3344;
    echo.available = 0xAABB_CCDD;
    echo.set_payload_size(8, 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    let payload: [u8; 8] = *b"PROBE!\0\0";
    let mut req = Vec::new();
    req.extend_from_slice(&echo.to_bytes());
    req.extend_from_slice(&payload);
    sock.write_all(&req).await.unwrap();

    // Read the response: 16-byte header + 8-byte payload = 24 bytes.
    let mut resp = [0u8; 64];
    let mut total = 0;
    while total < 24 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp[total..]))
            .await
            .expect("ECHO reply timed out")
            .expect("read ECHO reply");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(total >= 24, "expected 24 bytes, got {total}");

    let resp_hdr = CaHeader::from_bytes(&resp[..16]).expect("parse response header");
    assert_eq!(resp_hdr.cmmd, CA_PROTO_ECHO, "must echo CA_PROTO_ECHO");
    assert_eq!(
        resp_hdr.data_type, 0xAAAA,
        "must echo m_dataType; got 0x{:04x}",
        resp_hdr.data_type
    );
    assert_eq!(
        resp_hdr.cid, 0x1122_3344,
        "must echo m_cid; got 0x{:08x}",
        resp_hdr.cid
    );
    assert_eq!(
        resp_hdr.available, 0xAABB_CCDD,
        "must echo m_available; got 0x{:08x}",
        resp_hdr.available
    );
    assert_eq!(
        resp_hdr.postsize, 8,
        "must echo m_postsize; got {}",
        resp_hdr.postsize
    );
    assert_eq!(
        &resp[16..24],
        &payload,
        "must echo payload verbatim; got {:02x?}",
        &resp[16..24]
    );
}

/// C `event_cancel_reply` (`rsrv/camessage.c:1992-1996`)
/// calls `MPTOPCIU(mp)` first. If the request's channel id is
/// unknown or belongs to another client, rsrv calls `logBadId` —
/// which sends `send_err(ECA_INTERNAL, "Bad Resource ID")` with the
/// cid=0xFFFFFFFF sentinel (`camessage.c:307-320`), flushed by
/// `camsgtask.c:142` before the disconnect — and returns RSRV_ERROR.
/// Only after a valid channel resolves does rsrv walk that channel's
/// event queue and emit ECA_BADMONID for an unknown monitor id.
///
/// Pre-fix Rust checked the flat subscription map first, so an
/// unknown SID elicited ECA_BADMONID via the diagnostic fallback
/// path. This test asserts the bad-SID case replies ECA_INTERNAL
/// then disconnects (matches C `logBadId`); the valid-SID +
/// bad-sub_id case is covered by
/// `server_event_cancel_bad_subid_on_valid_sid_replies_eca_badmonid`.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_event_cancel_unknown_sid_replies_eca_internal_and_disconnects() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("BADMONID:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    // Connect a raw TCP socket and complete the CA handshake.
    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // Send VERSION (priority=0, minor=13).
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();

    // server emits an unsolicited VERSION when the circuit
    // becomes ready in addition to the handshake VERSION echo, so the
    // post-handshake drain must consume up to 2 VERSION frames
    // (32 bytes total) before the bad-SID cancel is written. Drain
    // until we've seen 32 bytes.
    let mut buf = [0u8; 64];
    let mut drained = 0usize;
    while drained < 32 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[drained..]))
            .await
            .expect("server VERSION reply timed out")
            .expect("read VERSION");
        if n == 0 {
            break;
        }
        drained += n;
    }
    assert!(
        drained >= 32,
        "expected 2 VERSION frames (32 bytes); got {drained} bytes"
    );

    // Send EVENT_CANCEL with an SID that was never opened. The server
    // must reply with the bad-SID CA_PROTO_ERROR(ECA_INTERNAL) frame,
    // then close the connection (C `event_cancel_reply` MPTOPCIU →
    // logBadId → send_err(ECA_INTERNAL) → RSRV_ERROR).
    let mut cancel = CaHeader::new(CA_PROTO_EVENT_CANCEL);
    cancel.data_type = 6; // DBR_DOUBLE
    cancel.count = 1;
    cancel.cid = 0xDEAD_BEEF; // bogus sid
    cancel.available = 0xCAFE_BABE; // bogus sub_id
    sock.write_all(&cancel.to_bytes()).await.unwrap();

    // Expect CA_PROTO_ERROR with ECA_INTERNAL + 0xFFFFFFFF sentinel cid.
    let mut resp = [0u8; 256];
    let mut total = 0;
    while total < 16 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp[total..]))
            .await
            .expect("server error-reply timed out")
            .expect("read error reply");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(
        total >= 16,
        "expected a CA_PROTO_ERROR header before disconnect, got {total} bytes"
    );
    let err_hdr = CaHeader::from_bytes(&resp[..16]).expect("parse error header");
    assert_eq!(err_hdr.cmmd, CA_PROTO_ERROR);
    assert_eq!(
        err_hdr.available, ECA_INTERNAL,
        "EVENT_CANCEL on an unknown SID takes the bad-SID logBadId branch \
         (ECA_INTERNAL); got eca={:#x}",
        err_hdr.available
    );
    assert_eq!(err_hdr.cid, 0xFFFF_FFFF);

    // Drain the trailing echo header + diagnostic string the server
    // queued before closing.
    let drain_start = total;
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        sock.read(&mut resp[drain_start..]),
    )
    .await;

    // Server must close the connection: a subsequent read returns EOF.
    let mut tail = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut tail))
        .await
        .expect("server did not close after EVENT_CANCEL bad-SID")
        .expect("read after bad-SID cancel");
    assert_eq!(
        n,
        0,
        "EVENT_CANCEL with unknown SID must close the connection after the \
         ECA_INTERNAL reply (matches C event_cancel_reply logBadId + RSRV_ERROR); \
         got {n} more bytes: {:02x?}",
        &tail[..n]
    );
}

/// C `bad_tcp_cmd_action` (`rsrv/camessage.c:337-352`) on an unknown
/// TCP command: (1) emit `CA_PROTO_ERROR` with `ECA_INTERNAL` and the
/// channel-cid 0xFFFFFFFF sentinel (per `vsend_err` non-channel-scoped
/// convention), then (2) return `RSRV_ERROR` so the dispatcher
/// (`camessage.c:2519-2524`) breaks out of the message loop, which
/// tears down the connection. The C source comment is explicit:
/// "by default, clients don't recover from this".
///
/// Pre-fix Rust handler emitted the CA_PROTO_ERROR but kept the
/// connection open — a malicious peer could flood the server with
/// unknown commands and force one reply per frame indefinitely. This
/// test verifies the server now drops the TCP connection after the
/// error reply.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_unknown_tcp_command_replies_error_and_disconnects() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("BAD:CMD", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // server now emits an unsolicited VERSION immediately
    // after accept (libca `rsrv_version_reply` parity). The client
    // therefore receives two CA_PROTO_VERSION frames before any
    // command-specific reply: one unsolicited, one in response to
    // our VERSION below. Drain both so the subsequent reads see
    // the unknown-cmd CA_PROTO_ERROR cleanly.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    let mut got = 0;
    while got < 32 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[got..]))
            .await
            .expect("server VERSION drain timed out")
            .expect("read VERSION");
        if n == 0 {
            break;
        }
        got += n;
    }
    assert!(
        got >= 32,
        "expected two CA_PROTO_VERSION frames; got {got} bytes"
    );

    // Send a TCP frame with an unknown command code. CA_PROTO_LAST_CMMD
    // in C is 27 (CA_PROTO_SERVER_DISCONN); 250 is comfortably past
    // every defined command across all minor versions.
    let mut unknown = CaHeader::new(250);
    unknown.cid = 0xDEAD_BEEF;
    sock.write_all(&unknown.to_bytes()).await.unwrap();

    // Expect CA_PROTO_ERROR with ECA_INTERNAL + 0xFFFFFFFF sentinel cid.
    let mut resp = [0u8; 256];
    let mut total = 0;
    while total < 16 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp[total..]))
            .await
            .expect("server error-reply timed out")
            .expect("read error reply");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(
        total >= 16,
        "expected at least one CA_PROTO_ERROR header before disconnect, got {total}"
    );
    let err_hdr = CaHeader::from_bytes(&resp[..16]).expect("parse error header");
    assert_eq!(err_hdr.cmmd, CA_PROTO_ERROR);
    assert_eq!(err_hdr.available, ECA_INTERNAL);
    assert_eq!(err_hdr.cid, 0xFFFF_FFFF);

    // Drain any trailing payload bytes the server queued before
    // closing (the original-header echo + diagnostic string).
    let drain_start = total;
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        sock.read(&mut resp[drain_start..]),
    )
    .await;

    // Server must close the connection: a subsequent read returns 0
    // (EOF) within a reasonable timeout.
    let mut tail = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut tail))
        .await
        .expect("server did not close TCP connection after unknown command")
        .expect("read after error");
    assert_eq!(
        n, 0,
        "server must drop the connection after CA_PROTO_ERROR on unknown command \
         (C bad_tcp_cmd_action parity); instead read {n} more bytes"
    );
}

/// C `tcp_version_action` (`rsrv/camessage.c:366-369`) rejects clients
/// whose minor version is below `CA_MINIMUM_SUPPORTED_VERSION` (= 4 per
/// `caProto.h:34`) by returning `RSRV_ERROR`, which tears down the TCP
/// connection. Without this gate an ancient peer could complete
/// VERSION and proceed to CREATE_CHAN with a wire format the modern
/// server no longer fully supports.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_tcp_version_below_minimum_drops_connection() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("VER:OLD", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // server emits an unsolicited VERSION on accept. Drain
    // exactly 16 bytes before sending our (unsupported) VERSION,
    // then verify the connection closes WITHOUT a second wire
    // frame (libca tcp_version_action parity: bad version
    // returns RSRV_ERROR which tears down with no reply).
    let mut buf = [0u8; 64];
    let mut greeting = [0u8; 16];
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut got = 0;
        while got < 16 {
            let n = sock.read(&mut greeting[got..]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok::<usize, std::io::Error>(got)
    })
    .await
    .expect("unsolicited VERSION timed out")
    .expect("read greeting");

    // CA V4.0 (minor = 0) is below CA_MINIMUM_SUPPORTED_VERSION = 4.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = 0;
    sock.write_all(&ver.to_bytes()).await.unwrap();

    // Server must drop the connection — no further VERSION reply,
    // just EOF.
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
        .await
        .expect("server did not close TCP after unsupported VERSION")
        .expect("read");
    assert_eq!(
        n, 0,
        "server must drop the connection on VERSION minor < 4 \
         (C tcp_version_action parity); instead read {n} bytes"
    );
}

/// C `write_notify_action` (`rsrv/camessage.c:1647-1651`) emits a
/// CA_PROTO_WRITE_NOTIFY error reply (`putNotifyErrorReply` with
/// `m_cid = ECA_BADTYPE`) when the WRITE_NOTIFY data type exceeds
/// `LAST_BUFFER_TYPE` (= DBR_CLASS_NAME = 38), then returns
/// `RSRV_ERROR` which tears the connection down. Pre-fix Rust sent
/// the error reply but kept the connection open, letting a peer
/// flood the server with bad-type WRITE_NOTIFYs.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_write_notify_bad_type_replies_error_and_disconnects() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("WRBAD:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // VERSION handshake
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut hello = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello))
        .await
        .expect("VERSION reply timed out")
        .expect("read VERSION");

    // CLIENT_NAME + HOST_NAME (required before CREATE_CHAN)
    for (cmd, name) in [
        (CA_PROTO_CLIENT_NAME, "testuser\0"),
        (CA_PROTO_HOST_NAME, "testhost\0"),
    ] {
        let mut h = CaHeader::new(cmd);
        let mut body = name.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        h.set_payload_size(body.len(), 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = Vec::new();
        frame.extend_from_slice(&h.to_bytes());
        frame.extend_from_slice(&body);
        sock.write_all(&frame).await.unwrap();
    }

    // CREATE_CHAN on WRBAD:PV to get a valid SID
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.cid = 0xCAFEBABE;
    let pv_name = b"WRBAD:PV\0";
    let mut create_body = pv_name.to_vec();
    while !create_body.len().is_multiple_of(8) {
        create_body.push(0);
    }
    create
        .set_payload_size(
            create_body.len(),
            0,
            epics_ca_rs::protocol::CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the extended header");
    let mut frame = Vec::new();
    frame.extend_from_slice(&create.to_bytes());
    frame.extend_from_slice(&create_body);
    sock.write_all(&frame).await.unwrap();

    // Drain server frames and walk header-by-header to find the
    // CREATE_CHAN reply. The server adds an unsolicited VERSION on
    // accept, so the byte offset of CREATE_CHAN is no longer
    // fixed (it depends on TCP segmentation + whether
    // ACCESS_RIGHTS lands separately). Scan instead of indexing.
    let mut buf = [0u8; 256];
    let mut got = 0;
    let create_resp = loop {
        let n = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[got..]))
            .await
            .expect("server drain timed out")
            .expect("read");
        if n == 0 {
            panic!("EOF before CREATE_CHAN reply");
        }
        got += n;
        // Walk 16-byte headers in `buf[..got]` looking for CREATE_CHAN.
        let mut off = 0;
        while off + 16 <= got {
            if let Ok(h) = CaHeader::from_bytes(&buf[off..off + 16]) {
                if h.cmmd == CA_PROTO_CREATE_CHAN {
                    break;
                }
                // Skip past header + padded postsize for the next walk.
                off += 16 + ((h.postsize as usize + 7) & !7);
            } else {
                off += 16;
            }
        }
        if off + 16 <= got {
            let h = CaHeader::from_bytes(&buf[off..off + 16]).unwrap();
            if h.cmmd == CA_PROTO_CREATE_CHAN {
                break h;
            }
        }
    };
    assert_eq!(create_resp.cmmd, CA_PROTO_CREATE_CHAN);
    let sid = create_resp.available;

    // Send WRITE_NOTIFY with data_type = 100 (well past
    // LAST_BUFFER_TYPE = 38). Payload size 0; C rejects on type
    // alone before reading payload.
    let mut bad = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    bad.data_type = 100;
    bad.count = 1;
    bad.cid = sid;
    bad.available = 0xDEAD_BEEF; // ioid
    sock.write_all(&bad.to_bytes()).await.unwrap();

    // Expect CA_PROTO_WRITE_NOTIFY error reply with cid = ECA_BADTYPE.
    let mut resp = [0u8; 64];
    let mut total = 0;
    while total < 16 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp[total..]))
            .await
            .expect("error reply timed out")
            .expect("read error reply");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(total >= 16, "expected error reply, got {total} bytes");
    let err_hdr = CaHeader::from_bytes(&resp[..16]).expect("parse error reply");
    assert_eq!(err_hdr.cmmd, CA_PROTO_WRITE_NOTIFY);
    assert_eq!(
        err_hdr.cid, ECA_BADTYPE,
        "cid carries ECA status per putNotifyErrorReply convention"
    );
    assert_eq!(err_hdr.available, 0xDEAD_BEEF, "ioid echoed");

    // Server must disconnect after the error reply.
    let mut tail = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut tail))
        .await
        .expect("server did not close after WRITE_NOTIFY bad type")
        .expect("read after error");
    assert_eq!(
        n, 0,
        "server must drop the connection after WRITE_NOTIFY bad type \
         (C write_notify_action RSRV_ERROR parity); got {n} more bytes"
    );
}

/// Deprecated synchronous `CA_PROTO_READ` (cmd 3) sizes EVERY reply
/// with `dbr_size_n(type, m_count)` and writes the header count as
/// `m_count` VERBATIM — there is NO DBR_CLASS_NAME special case in C
/// `read_action` (`rsrv/camessage.c:622-624`). For DBR_CLASS_NAME (38)
/// with `m_count == 0`, `dbr_size_n(38, 0) = dbr_size[38] -
/// dbr_value_size[38] = 40 - 40 = 0` (`access.cpp:906`/`:955`,
/// `db_access.h:533`), so the reply ships `count = 0` and a 0-byte
/// payload. Only the READ_NOTIFY / EVENT_ADD path (C `read_reply`,
/// `camessage.c:507-575`) treats `m_count == 0` as autosize and forces
/// the fixed 40-byte class string at `count = 1`.
///
/// Pre-fix Rust normalized DBR_CLASS_NAME to `count = 1` BEFORE the
/// deprecated `count == 0` branch, so a deprecated READ of
/// DBR_CLASS_NAME at count 0 wrongly shipped `count = 1` + 40 bytes,
/// diverging from rsrv on both the wire count field and payload length.
///
/// Tests the invariant boundary, not one scenario:
///   A. deprecated READ, CLASS_NAME, count 0  -> count 0, 0-byte body
///   B. deprecated READ, CLASS_NAME, count 1  -> count 1, 40-byte body
///   C. READ_NOTIFY,     CLASS_NAME, count 0  -> count 1, 40-byte body
/// B and C pin that the reorder preserves ordinary class-name reads and
/// the notify autosize path.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_deprecated_read_class_name_count0_follows_c_m_count() {
    use std::collections::HashMap as Map;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // DBR_CLASS_NAME = 38 (epics_base_rs::types::DBR_CLASS_NAME); use the
    // literal to match this file's wire-level convention (no types import).
    const DBR_CLASS_NAME: u16 = 38;

    let server = CaServer::builder()
        .port(0)
        .pv("CLSNM:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // VERSION handshake
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut hello = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello))
        .await
        .expect("VERSION reply timed out")
        .expect("read VERSION");

    // CLIENT_NAME + HOST_NAME (required before CREATE_CHAN)
    for (cmd, name) in [
        (CA_PROTO_CLIENT_NAME, "testuser\0"),
        (CA_PROTO_HOST_NAME, "testhost\0"),
    ] {
        let mut h = CaHeader::new(cmd);
        let mut body = name.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        h.set_payload_size(body.len(), 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = Vec::new();
        frame.extend_from_slice(&h.to_bytes());
        frame.extend_from_slice(&body);
        sock.write_all(&frame).await.unwrap();
    }

    // CREATE_CHAN on CLSNM:PV with a distinctive client CID. The
    // deprecated READ reply must echo this CID (`pciu->cid`), so capture
    // it to assert on later.
    const CLIENT_CID: u32 = 0xCAFE_BABE;
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.cid = CLIENT_CID;
    let pv_name = b"CLSNM:PV\0";
    let mut create_body = pv_name.to_vec();
    while !create_body.len().is_multiple_of(8) {
        create_body.push(0);
    }
    create
        .set_payload_size(
            create_body.len(),
            0,
            epics_ca_rs::protocol::CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the extended header");
    let mut frame = Vec::new();
    frame.extend_from_slice(&create.to_bytes());
    frame.extend_from_slice(&create_body);
    sock.write_all(&frame).await.unwrap();

    // Scan server frames header-by-header to find the CREATE_CHAN reply
    // (the SID lives in m_available). Frame order is not fixed (VERSION,
    // ACCESS_RIGHTS may interleave), so walk rather than index.
    let mut buf = [0u8; 256];
    let mut got = 0;
    let sid = loop {
        let n = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[got..]))
            .await
            .expect("server drain timed out")
            .expect("read");
        if n == 0 {
            panic!("EOF before CREATE_CHAN reply");
        }
        got += n;
        let mut off = 0;
        let mut found = None;
        while off + 16 <= got {
            if let Ok(h) = CaHeader::from_bytes(&buf[off..off + 16]) {
                if h.cmmd == CA_PROTO_CREATE_CHAN {
                    found = Some(h.available);
                    break;
                }
                off += 16 + ((h.postsize as usize + 7) & !7);
            } else {
                off += 16;
            }
        }
        if let Some(sid) = found {
            break sid;
        }
    };

    // Three reads, distinct ioids in m_available so replies can be keyed.
    // ioid 0x0A: deprecated READ (cmd 3), CLASS_NAME, count 0
    // ioid 0x0B: deprecated READ (cmd 3), CLASS_NAME, count 1
    // ioid 0x0C: READ_NOTIFY (cmd 15), CLASS_NAME, count 0
    for (cmd, count, ioid) in [
        (CA_PROTO_READ, 0u16, 0x0A_u32),
        (CA_PROTO_READ, 1u16, 0x0B_u32),
        (CA_PROTO_READ_NOTIFY, 0u16, 0x0C_u32),
    ] {
        let mut r = CaHeader::new(cmd);
        r.data_type = DBR_CLASS_NAME;
        r.count = count;
        r.cid = sid; // request addresses the channel by SID
        r.available = ioid;
        sock.write_all(&r.to_bytes()).await.unwrap();
    }

    // Drain replies and collect READ / READ_NOTIFY headers by ioid.
    let mut acc: Vec<u8> = Vec::new();
    let mut replies: Map<u32, CaHeader> = Map::new();
    let mut rbuf = [0u8; 256];
    let deadline = Duration::from_secs(3);
    while replies.len() < 3 {
        let n = tokio::time::timeout(deadline, sock.read(&mut rbuf))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for READ replies; got {:?}",
                    replies.keys()
                )
            })
            .expect("read replies");
        if n == 0 {
            panic!("EOF before all READ replies; got {:?}", replies.keys());
        }
        acc.extend_from_slice(&rbuf[..n]);
        let mut off = 0;
        while off + 16 <= acc.len() {
            let Ok(h) = CaHeader::from_bytes(&acc[off..off + 16]) else {
                off += 16;
                continue;
            };
            if (h.cmmd == CA_PROTO_READ || h.cmmd == CA_PROTO_READ_NOTIFY)
                && [0x0A, 0x0B, 0x0C].contains(&h.available)
            {
                replies.insert(h.available, h);
            }
            off += 16 + ((h.postsize as usize + 7) & !7);
        }
    }

    // A. deprecated READ, CLASS_NAME, count 0 -> count 0, 0-byte payload,
    //    m_cid echoes the client CID (`pciu->cid`).
    let a = &replies[&0x0A];
    assert_eq!(a.cmmd, CA_PROTO_READ, "A: deprecated READ reply opcode");
    assert_eq!(
        a.count, 0,
        "A: deprecated CA_PROTO_READ DBR_CLASS_NAME count=0 must ship \
         header count=0 (C read_action writes m_count verbatim), got {}",
        a.count
    );
    assert_eq!(
        a.postsize, 0,
        "A: dbr_size_n(DBR_CLASS_NAME, 0) = 40 - 40 = 0, so the payload \
         must be 0 bytes, got postsize={}",
        a.postsize
    );
    assert_eq!(
        a.cid, CLIENT_CID,
        "A: deprecated READ m_cid carries pciu->cid (client CID), not the SID"
    );

    // B. deprecated READ, CLASS_NAME, count 1 -> count 1, 40-byte payload.
    let b = &replies[&0x0B];
    assert_eq!(b.cmmd, CA_PROTO_READ, "B: deprecated READ reply opcode");
    assert_eq!(
        b.count, 1,
        "B: ordinary deprecated CLASS_NAME read (count!=0) stays count=1"
    );
    assert_eq!(
        b.postsize, 40,
        "B: CLASS_NAME at count=1 is the fixed 40-byte string"
    );

    // C. READ_NOTIFY, CLASS_NAME, count 0 -> count 1, 40-byte payload
    //    (the notify autosize path is unaffected by the reorder).
    let c = &replies[&0x0C];
    assert_eq!(c.cmmd, CA_PROTO_READ_NOTIFY, "C: READ_NOTIFY reply opcode");
    assert_eq!(
        c.count, 1,
        "C: READ_NOTIFY CLASS_NAME count=0 autosizes to count=1 (read_reply)"
    );
    assert_eq!(
        c.postsize, 40,
        "C: READ_NOTIFY CLASS_NAME ships the fixed 40-byte string"
    );
}

/// Deprecated synchronous `CA_PROTO_READ` (cmd 3) contracts a scalar
/// `DBR_STRING` payload to its NUL-terminated length. C `read_action`
/// (`rsrv/camessage.c:666-680`) recomputes `payloadSize =
/// epicsStrnLen(pStr, 40) + 1` for `DBR_STRING && m_count == 1`, then
/// `cas_commit_msg` (`caserverio.c:350-365`) aligns to 8 and rewrites
/// `m_postsize` (header count stays 1). So `"OK"` commits an 8-byte
/// payload, not the fixed 40-byte slot. READ_NOTIFY / EVENT_ADD never
/// run this branch (C `read_reply` keeps the full slot).
///
/// Boundary cases in one test:
///   A. deprecated READ, "OK"           -> count 1, postsize 8  (align8(2+1))
///   B. deprecated READ, 40-char value  -> count 1, postsize 40 (39 chars + NUL)
///   C. READ_NOTIFY,     "OK"           -> count 1, postsize 40 (full slot)
/// B pins that a near-full string is not over-trimmed; C pins that the
/// notify path keeps the C `read_reply` 40-byte slot.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_deprecated_read_string_shortens_to_nul_length() {
    use std::collections::HashMap as Map;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // DBR_STRING = 0 (epics_base_rs::types::DBR_STRING).
    const DBR_STRING: u16 = 0;

    // STR:SHORT = "OK"; STR:LONG = 40 'A's (encoder clamps to 39 chars +
    // a NUL at byte 39, so epicsStrnLen == 39 and the trimmed size is 40).
    let server = CaServer::builder()
        .port(0)
        .pv("STR:SHORT", EpicsValue::String("OK".into()))
        .pv("STR:LONG", EpicsValue::String("A".repeat(40).into()))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // VERSION handshake
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut hello = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello))
        .await
        .expect("VERSION reply timed out")
        .expect("read VERSION");

    // CLIENT_NAME + HOST_NAME
    for (cmd, name) in [
        (CA_PROTO_CLIENT_NAME, "testuser\0"),
        (CA_PROTO_HOST_NAME, "testhost\0"),
    ] {
        let mut h = CaHeader::new(cmd);
        let mut body = name.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        h.set_payload_size(body.len(), 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = Vec::new();
        frame.extend_from_slice(&h.to_bytes());
        frame.extend_from_slice(&body);
        sock.write_all(&frame).await.unwrap();
    }

    // CREATE_CHAN on both PVs; capture (client_cid -> sid).
    let mut want_create: Map<u32, &str> = Map::new();
    want_create.insert(0x1111_0001, "STR:SHORT\0");
    want_create.insert(0x1111_0002, "STR:LONG\0");
    for (cid, pv) in [(0x1111_0001u32, "STR:SHORT\0"), (0x1111_0002, "STR:LONG\0")] {
        let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
        create.cid = cid;
        let mut body = pv.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        create
            .set_payload_size(body.len(), 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = Vec::new();
        frame.extend_from_slice(&create.to_bytes());
        frame.extend_from_slice(&body);
        sock.write_all(&frame).await.unwrap();
    }

    // Drain CREATE_CHAN replies; map client cid -> sid (m_cid -> m_available).
    let mut acc: Vec<u8> = Vec::new();
    let mut sids: Map<u32, u32> = Map::new();
    let mut buf = [0u8; 512];
    while sids.len() < 2 {
        let n = tokio::time::timeout(Duration::from_millis(800), sock.read(&mut buf))
            .await
            .expect("CREATE_CHAN drain timed out")
            .expect("read");
        if n == 0 {
            panic!("EOF before both CREATE_CHAN replies; got {:?}", sids.keys());
        }
        acc.extend_from_slice(&buf[..n]);
        let mut off = 0;
        while off + 16 <= acc.len() {
            let Ok(h) = CaHeader::from_bytes(&acc[off..off + 16]) else {
                off += 16;
                continue;
            };
            if h.cmmd == CA_PROTO_CREATE_CHAN && want_create.contains_key(&h.cid) {
                sids.insert(h.cid, h.available);
            }
            off += 16 + ((h.postsize as usize + 7) & !7);
        }
    }
    let sid_short = sids[&0x1111_0001];
    let sid_long = sids[&0x1111_0002];

    // ioid 0x21: deprecated READ (cmd 3) DBR_STRING count=1 on "OK"
    // ioid 0x22: deprecated READ (cmd 3) DBR_STRING count=1 on 40-char
    // ioid 0x23: READ_NOTIFY (cmd 15) DBR_STRING count=1 on "OK"
    for (cmd, sid, ioid) in [
        (CA_PROTO_READ, sid_short, 0x21_u32),
        (CA_PROTO_READ, sid_long, 0x22_u32),
        (CA_PROTO_READ_NOTIFY, sid_short, 0x23_u32),
    ] {
        let mut r = CaHeader::new(cmd);
        r.data_type = DBR_STRING;
        r.count = 1;
        r.cid = sid;
        r.available = ioid;
        sock.write_all(&r.to_bytes()).await.unwrap();
    }

    // Collect the three READ / READ_NOTIFY replies by ioid.
    let mut racc: Vec<u8> = Vec::new();
    let mut replies: Map<u32, CaHeader> = Map::new();
    let mut rbuf = [0u8; 512];
    while replies.len() < 3 {
        let n = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut rbuf))
            .await
            .unwrap_or_else(|_| panic!("timed out; got {:?}", replies.keys()))
            .expect("read replies");
        if n == 0 {
            panic!("EOF before all replies; got {:?}", replies.keys());
        }
        racc.extend_from_slice(&rbuf[..n]);
        let mut off = 0;
        while off + 16 <= racc.len() {
            let Ok(h) = CaHeader::from_bytes(&racc[off..off + 16]) else {
                off += 16;
                continue;
            };
            if (h.cmmd == CA_PROTO_READ || h.cmmd == CA_PROTO_READ_NOTIFY)
                && [0x21, 0x22, 0x23].contains(&h.available)
            {
                replies.insert(h.available, h);
            }
            off += 16 + ((h.postsize as usize + 7) & !7);
        }
    }

    // A. "OK" -> count 1, postsize align8(2+1) = 8.
    let a = &replies[&0x21];
    assert_eq!(a.cmmd, CA_PROTO_READ, "A: deprecated READ reply opcode");
    assert_eq!(a.count, 1, "A: scalar DBR_STRING is one element");
    assert_eq!(
        a.postsize, 8,
        "A: \"OK\" contracts to epicsStrnLen+1=3 aligned to 8, got {}",
        a.postsize
    );

    // B. 40-char value -> 39 chars + NUL = 40 bytes, align8(40) = 40.
    let b = &replies[&0x22];
    assert_eq!(b.cmmd, CA_PROTO_READ, "B: deprecated READ reply opcode");
    assert_eq!(b.count, 1, "B: scalar DBR_STRING is one element");
    assert_eq!(
        b.postsize, 40,
        "B: a near-full (39-char + NUL) string keeps the 40-byte slot, got {}",
        b.postsize
    );

    // C. READ_NOTIFY "OK" -> full 40-byte slot (no read_action shortening).
    let c = &replies[&0x23];
    assert_eq!(c.cmmd, CA_PROTO_READ_NOTIFY, "C: READ_NOTIFY reply opcode");
    assert_eq!(c.count, 1, "C: scalar DBR_STRING is one element");
    assert_eq!(
        c.postsize, 40,
        "C: READ_NOTIFY keeps the full 40-byte DBR_STRING slot (C read_reply), got {}",
        c.postsize
    );
}

/// The UDP batching loop must not re-parse a stale
/// datagram. When `try_recv_from` drains a queued datagram that is
/// rejected (short sub-header, ignore-list, or rate-limited), pre-fix
/// Rust `continue 'parse`d WITHOUT replacing `current_buf` — so the
/// *previous* datagram's bytes were parsed a second time. For a
/// short queued datagram the prior client's SEARCH was reprocessed
/// and its reply duplicated to that same client; for an ignore/rate-
/// limit reject the peer-change branch had already repointed
/// `current_src`, so the reply went to the wrong address.
///
/// C `cast_server.c:163-281` does one `recvfrom` per loop iteration,
/// always overwriting `client->recv.buf` before `camessage()` runs.
/// The Rust drain loop must likewise discard a rejected queued
/// datagram without re-parsing the old buffer.
///
/// Test: burst N valid SEARCHes (each with a unique client cid)
/// interleaved with short junk datagrams so the server's
/// `try_recv_from` peek reliably finds a queued short datagram while
/// the prior SEARCH is still in `current_buf`. Every cid must be
/// answered exactly once; pre-fix a re-parse duplicates a cid's
/// SEARCH reply.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mr_r7_rejected_queued_datagram_does_not_reparse_stale_buffer() {
    use std::collections::HashMap as Map;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    let server = CaServer::builder()
        .port(0)
        .pv("MR7:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    // This test speaks UDP SEARCH straight at the responder, so it needs
    // the UDP port, not the TCP one.
    let port = server.udp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    // Build a CA_PROTO_SEARCH UDP datagram for MR7:PV. The server's
    // SEARCH reply echoes `m_available` (the client cid), so a unique
    // cid per request lets us detect a duplicated answer.
    let search_datagram = |cid: u32| -> Vec<u8> {
        let pv = b"MR7:PV";
        let mut padded = pv.to_vec();
        padded.push(0);
        while !padded.len().is_multiple_of(8) {
            padded.push(0);
        }
        let mut h = CaHeader::new(CA_PROTO_SEARCH);
        h.postsize = padded.len() as u16;
        h.data_type = CA_DO_REPLY;
        h.count = CA_MINOR_VERSION;
        h.cid = cid;
        h.available = cid;
        let mut bytes = h.to_bytes().to_vec();
        bytes.extend_from_slice(&padded);
        bytes
    };

    let server_addr = ("127.0.0.1", port);
    let peer = UdpSocket::bind(("127.0.0.1", 0)).await.expect("bind peer");

    // Burst N (valid SEARCH, short junk) pairs from the SAME peer so
    // they queue in the server's kernel recv buffer. The burst
    // outpaces the server's one-datagram-per-`recv_from` processing,
    // so when the server peeks via `try_recv_from` it finds the
    // queued short junk datagram while the prior SEARCH is still in
    // `current_buf`.
    //
    // UDP makes no delivery promise under this burst: 600 datagrams
    // can overflow the server socket's kernel recv queue
    // (net.core.rmem_default ≈ 212 KB < 600 × skb truesize) and the
    // kernel legitimately drops the excess — that is loss, not the
    // re-parse defect this test pins. Real CA clients retry SEARCH
    // (`searchTimer`), so unanswered slots are retried with FRESH cids
    // each round: a kernel-dropped request cannot fail the test, while
    // any single cid answered twice (the actual R7 regression) still
    // can, because no cid is ever sent twice.
    const N: usize = 300;
    let mut answered = [false; N];
    let mut cid_to_slot: Map<u32, usize> = Map::new();
    // Per-cid reply count, across every cid ever sent (dup check).
    let mut counts: Map<u32, u32> = Map::new();
    let mut next_cid = 0xC000_0000u32;
    let mut rbuf = [0u8; 64 * 1024];

    'rounds: for _round in 0..5 {
        for (slot, done) in answered.iter().enumerate() {
            if *done {
                continue;
            }
            let cid = next_cid;
            next_cid += 1;
            cid_to_slot.insert(cid, slot);
            peer.send_to(&search_datagram(cid), server_addr)
                .await
                .expect("send SEARCH");
            // 8-byte datagram: below the 16-byte CA header size.
            peer.send_to(&[0u8; 8], server_addr)
                .await
                .expect("send short junk datagram");
        }

        // Collect this round's replies for up to ~1.5s, walking each
        // 16-byte header and counting SEARCH replies per echoed cid.
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), peer.recv_from(&mut rbuf)).await
            {
                Ok(Ok((got, _from))) => {
                    let mut off = 0;
                    while off + CaHeader::SIZE <= got {
                        let Ok(h) = CaHeader::from_bytes(&rbuf[off..off + CaHeader::SIZE]) else {
                            break;
                        };
                        if h.cmmd == CA_PROTO_SEARCH {
                            *counts.entry(h.available).or_insert(0) += 1;
                            if let Some(&slot) = cid_to_slot.get(&h.available) {
                                answered[slot] = true;
                            }
                        }
                        off += CaHeader::SIZE + ((h.postsize as usize + 7) & !7);
                    }
                }
                // C client parity (`udpiiu.cpp:420-426`): UDP recv errors
                // `ECONNRESET` (Windows KB263823 — an earlier send drew an
                // ICMP port-unreachable) and `ECONNREFUSED` (Linux
                // equivalent) are ignored and receiving continues; libca
                // never fails on them.
                Ok(Err(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue;
                }
                Ok(Err(e)) => panic!("peer recv error: {e}"),
                Err(_) => {
                    if answered.iter().all(|&a| a) {
                        break;
                    }
                }
            }
        }
        if answered.iter().all(|&a| a) {
            break 'rounds;
        }
    }

    // No cid may be answered more than once. A cid answered twice means
    // the server re-parsed a stale `current_buf` after draining a
    // rejected short datagram — every cid is sent exactly once, so
    // retries cannot produce a legitimate duplicate.
    let duplicated: Vec<(u32, u32)> = counts
        .iter()
        .filter(|&(_, &c)| c > 1)
        .map(|(&cid, &c)| (cid, c))
        .collect();
    assert!(
        duplicated.is_empty(),
        "{} SEARCH cid(s) answered more than once — stale `current_buf` \
         reparsed after a rejected queued datagram: {:?}",
        duplicated.len(),
        duplicated,
    );
    // Every slot must be answered within the retry budget. Persistent
    // non-answers (with retries absorbing kernel drops) mean the
    // responder died or stopped serving — e.g. the pre-fix cast server
    // exiting its recv loop on a transient UDP recv error.
    let unanswered = answered.iter().filter(|&&a| !a).count();
    assert_eq!(
        unanswered, 0,
        "{unanswered} of {N} SEARCH slots never answered across 5 send \
         rounds — responder lost or stopped serving"
    );
}

/// C `read_notify_action` (`rsrv/camessage.c:693-697`): `INVALID_DB_REQ`
/// (data_type > LAST_BUFFER_TYPE = 38) returns RSRV_ERROR WITHOUT
/// emitting any wire frame — only the deprecated `read_action`
/// (`camessage.c:616-620`) calls `send_err(ECA_BADTYPE)` here.
/// pre-fix Rust sent a CA_PROTO_READ_NOTIFY error frame for
/// the notify path too, an extra wire frame before EOF that rsrv
/// never produces. Test asserts the silent-close behaviour: no
/// wire frame, just connection drop.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_read_notify_bad_type_closes_silently() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("RDBAD:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // VERSION + CLIENT_NAME + HOST_NAME + CREATE_CHAN handshake.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    // drain BOTH server VERSION frames (unsolicited + reply).
    let mut hello = [0u8; 64];
    let mut got_hello = 0;
    while got_hello < 32 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello[got_hello..]))
            .await
            .expect("VERSION drain timed out")
            .expect("read VERSION");
        if n == 0 {
            break;
        }
        got_hello += n;
    }
    for (cmd, name) in [
        (CA_PROTO_CLIENT_NAME, "testuser\0"),
        (CA_PROTO_HOST_NAME, "testhost\0"),
    ] {
        let mut h = CaHeader::new(cmd);
        let mut body = name.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        h.set_payload_size(body.len(), 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        let mut frame = Vec::new();
        frame.extend_from_slice(&h.to_bytes());
        frame.extend_from_slice(&body);
        sock.write_all(&frame).await.unwrap();
    }
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.cid = 0xC0FFEEEE;
    let pv_name = b"RDBAD:PV\0";
    let mut create_body = pv_name.to_vec();
    while !create_body.len().is_multiple_of(8) {
        create_body.push(0);
    }
    create
        .set_payload_size(
            create_body.len(),
            0,
            epics_ca_rs::protocol::CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the extended header");
    let mut frame = Vec::new();
    frame.extend_from_slice(&create.to_bytes());
    frame.extend_from_slice(&create_body);
    sock.write_all(&frame).await.unwrap();
    // Walk frames to find CREATE_CHAN reply (offsets shifted).
    let mut buf = [0u8; 256];
    let mut got = 0;
    let sid = loop {
        let n = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf[got..]))
            .await
            .expect("server drain timed out")
            .expect("read");
        if n == 0 {
            panic!("EOF before CREATE_CHAN reply");
        }
        got += n;
        let mut off = 0;
        let mut found = None;
        while off + 16 <= got {
            if let Ok(h) = CaHeader::from_bytes(&buf[off..off + 16]) {
                if h.cmmd == CA_PROTO_CREATE_CHAN {
                    found = Some(h.available);
                    break;
                }
                off += 16 + ((h.postsize as usize + 7) & !7);
            } else {
                off += 16;
            }
        }
        if let Some(v) = found {
            break v;
        }
    };

    // READ_NOTIFY with data_type = 200 (well past LAST_BUFFER_TYPE = 38).
    let mut bad = CaHeader::new(CA_PROTO_READ_NOTIFY);
    bad.data_type = 200;
    bad.count = 1;
    bad.cid = sid;
    bad.available = 0xFADE_FADE; // ioid
    sock.write_all(&bad.to_bytes()).await.unwrap();

    // server must drop the connection WITHOUT emitting a
    // wire frame — C `read_notify_action` returns RSRV_ERROR
    // silently on INVALID_DB_REQ. Reading should observe EOF
    // (n=0) directly, never any header bytes.
    let mut resp = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp))
        .await
        .expect("server did not close after READ_NOTIFY bad type")
        .expect("read after bad type");
    assert_eq!(
        n,
        0,
        "READ_NOTIFY bad-type must elicit a silent close (matches \
         C read_notify_action RSRV_ERROR); got {n} bytes: {:02x?}",
        &resp[..n]
    );
}

/// C `read_sync_reply` (`rsrv/camessage.c:2053-2067`) echoes the
/// request header back with cmmd=CA_PROTO_READ_SYNC, m_postsize=0,
/// and the request's m_dataType / m_count / m_cid / m_available
/// preserved. libca client treats this as ECHO (`cac.cpp:72-73`).
/// Pre-fix Rust silently no-op-ed; this regression test ensures the
/// echo reply now arrives with the expected fields.
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_read_sync_echoes_request_header() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let server = CaServer::builder()
        .port(0)
        .pv("SYNC:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    // drain both VERSION frames (unsolicited + reply).
    let mut hello = [0u8; 64];
    let mut got_hello = 0;
    while got_hello < 32 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello[got_hello..]))
            .await
            .expect("VERSION drain timed out")
            .expect("read VERSION");
        if n == 0 {
            break;
        }
        got_hello += n;
    }
    assert!(
        got_hello >= 32,
        "expected 2 VERSION frames; got {got_hello}"
    );

    // Send READ_SYNC with distinctive field values to verify echo.
    let mut sync = CaHeader::new(CA_PROTO_READ_SYNC);
    sync.data_type = 0xBEEF;
    sync.count = 0x1234;
    sync.cid = 0xCAFE_F00D;
    sync.available = 0xDEAD_BEEF;
    sock.write_all(&sync.to_bytes()).await.unwrap();

    // Expect CA_PROTO_READ_SYNC reply with the same fields echoed.
    let mut resp = [0u8; 32];
    let mut total = 0;
    while total < 16 {
        let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp[total..]))
            .await
            .expect("READ_SYNC echo timed out")
            .expect("read echo");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(total >= 16, "expected echo reply, got {total} bytes");
    let echo = CaHeader::from_bytes(&resp[..16]).expect("parse echo");
    assert_eq!(echo.cmmd, CA_PROTO_READ_SYNC);
    assert_eq!(echo.data_type, 0xBEEF, "m_dataType echoed");
    assert_eq!(echo.count, 0x1234, "m_count echoed");
    assert_eq!(echo.cid, 0xCAFE_F00D, "m_cid echoed");
    assert_eq!(echo.available, 0xDEAD_BEEF, "m_available echoed");
    assert_eq!(echo.postsize, 0, "no payload");
}

// ---------------------------------------------------------------------------
// NativeTypeChanged is a *transition* signal, not a *discovery* signal
// ---------------------------------------------------------------------------

/// On the very first connect a channel has no prior native DBR type, so the
/// type is being discovered — the `Connected` event already carries it. The
/// client must NOT also emit `NativeTypeChanged` here: doing so makes every
/// first connect look like a type change and pushes consumers into a redundant
/// metadata refetch (and, if their connect handler is not idempotent, a
/// duplicate initial value). `NativeTypeChanged` is reserved for a genuine
/// transition from a known prior type (an IOC redefining the record, or a
/// reconnect to a differently-typed record).
// Async `CaServer` path: no tokio reactor under `rtems-exec-model`.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test]
async fn first_connect_does_not_emit_native_type_changed() {
    use std::time::Duration;

    use epics_ca_rs::client::{CaClient, ConnectionEvent};

    let server = CaServer::builder()
        .port(0)
        .pv("NTC:FIRST:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _server_handle = tokio::spawn(async move { server.run().await });

    // Target the in-process server directly (no process-global env mutation),
    // so this test needs no `#[serial]` and cannot race the env other tests set.
    let client = CaClient::new().await.expect("client");
    client.add_address(([127, 0, 0, 1], port).into());
    let channel = client.create_channel("NTC:FIRST:PV");
    // Subscribe to the event stream synchronously, before the async connect can
    // complete: the connect involves a UDP search + TCP handshake (ms), this
    // subscribe is µs, so `Connected` cannot slip past. The `saw_connected`
    // assertion below fails loudly if it ever does, so a missed window can never
    // masquerade as "no NativeTypeChanged".
    let mut events = channel.connection_events();

    channel
        .wait_connected(Duration::from_secs(5))
        .await
        .expect("channel connects to the in-process server");

    // Drain events up to and just past connect. `Connected` (and
    // `AccessRightsChanged`) are expected; `NativeTypeChanged` is not. If the
    // client wrongly emitted it, it lands right after `Connected` in the same
    // burst, well within this idle window.
    let mut saw_connected = false;
    loop {
        match tokio::time::timeout(Duration::from_millis(300), events.recv()).await {
            Ok(Ok(ev)) => {
                if matches!(ev, ConnectionEvent::Connected) {
                    saw_connected = true;
                }
                assert!(
                    !matches!(ev, ConnectionEvent::NativeTypeChanged { .. }),
                    "first connect must not emit NativeTypeChanged: the native \
                     type was discovered, not changed"
                );
            }
            // Lagged or closed: no more meaningful events to inspect.
            Ok(Err(_)) => break,
            // Idle window elapsed with no further event — the burst is over.
            Err(_) => break,
        }
    }

    assert!(
        saw_connected,
        "expected a Connected event on first connect (else the event window was \
         missed and the NativeTypeChanged check is meaningless)"
    );
}
