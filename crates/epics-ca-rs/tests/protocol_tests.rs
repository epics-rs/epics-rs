//! Integration tests for epics-ca-rs: protocol encoding/decoding and server API.

use std::collections::HashMap;

use epics_ca_rs::EpicsValue;
use epics_ca_rs::protocol::*;
use epics_ca_rs::server::CaServer;
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
    hdr.set_payload_size(200_000, 25_000);

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
    hdr.set_payload_size(500, 10);
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
    hdr.set_payload_size(0xFFFE, 1);
    assert!(!hdr.is_extended());
    assert_eq!(hdr.postsize, 0xFFFE);

    // 0xFFFF triggers extended
    hdr.set_payload_size(0xFFFF, 1);
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_postsize(), 0xFFFF);
}

#[test]
fn header_set_payload_count_boundary_at_0xffff() {
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);

    // count = 0xFFFE fits in normal form
    hdr.set_payload_size(100, 0xFFFE);
    assert!(!hdr.is_extended());
    assert_eq!(hdr.count, 0xFFFE);

    // count = 0xFFFF triggers extended (C `comQueSend.cpp:285` —
    // `nElem < 0xffff` is the normal threshold, so exact `0xFFFF`
    // requires extended form).
    hdr.set_payload_size(100, 0xFFFF);
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_count(), 0xFFFF);

    // count = 0x10000 triggers extended
    hdr.set_payload_size(100, 0x10000);
    assert!(hdr.is_extended());
    assert_eq!(hdr.actual_count(), 0x10000);
}

// ---------------------------------------------------------------------------
// CaServer builder pattern — basic construction with simple PVs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_builder_with_simple_pvs() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_put_and_get_double() {
    let server = CaServer::builder()
        .pv("SRV:D", EpicsValue::Double(0.0))
        .build()
        .await
        .unwrap();

    server.put("SRV:D", EpicsValue::Double(99.9)).await.unwrap();
    assert_eq!(server.get("SRV:D").await.unwrap(), EpicsValue::Double(99.9));
}

#[tokio::test]
async fn server_put_and_get_string() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_put_and_get_short() {
    let server = CaServer::builder()
        .pv("SRV:I", EpicsValue::Short(0))
        .build()
        .await
        .unwrap();

    server.put("SRV:I", EpicsValue::Short(-123)).await.unwrap();
    assert_eq!(server.get("SRV:I").await.unwrap(), EpicsValue::Short(-123));
}

#[tokio::test]
async fn server_put_and_get_enum() {
    let server = CaServer::builder()
        .pv("SRV:E", EpicsValue::Enum(0))
        .build()
        .await
        .unwrap();

    server.put("SRV:E", EpicsValue::Enum(5)).await.unwrap();
    assert_eq!(server.get("SRV:E").await.unwrap(), EpicsValue::Enum(5));
}

#[tokio::test]
async fn server_put_and_get_float() {
    let server = CaServer::builder()
        .pv("SRV:F", EpicsValue::Float(0.0))
        .build()
        .await
        .unwrap();

    server.put("SRV:F", EpicsValue::Float(2.5)).await.unwrap();
    assert_eq!(server.get("SRV:F").await.unwrap(), EpicsValue::Float(2.5));
}

#[tokio::test]
async fn server_put_and_get_long() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_put_and_get_char() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_get_nonexistent_pv_returns_error() {
    let server = CaServer::builder()
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_stats_bytes_in_out_track_real_traffic() {
    use std::time::Duration;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("STATS:BYTES", EpicsValue::Double(7.5))
        .build()
        .await
        .expect("build CA server");
    let stats = server.stats();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    // Let the listener bind + accept loop spin up.
    tokio::time::sleep(Duration::from_millis(200)).await;

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_stats_subscription_counters_track_camonitor_lifecycle() {
    use std::time::Duration;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("STATS:SUB:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let stats = server.stats();
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

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

#[tokio::test]
async fn server_put_nonexistent_pv_returns_error() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_add_pv_at_runtime() {
    let server = CaServer::builder().build().await.unwrap();

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

#[tokio::test]
async fn server_multiple_pv_types_coexist() {
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_builder_db_string_ai_record() {
    let db_text = r#"
record(ai, "TEMP:READING") {
    field(VAL, "25.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
        .db_string(db_text, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let val = server.get("TEMP:READING").await.unwrap();
    assert_eq!(val, EpicsValue::Double(25.0));
}

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

#[tokio::test]
async fn server_put_to_record() {
    let db_text = r#"
record(ao, "CTRL:SP") {
    field(VAL, "0.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_mixed_simple_pvs_and_records() {
    let db_text = r#"
record(ai, "REC:AI") {
    field(VAL, "10.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServer::builder()
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

#[tokio::test]
async fn server_database_accessor() {
    let server = CaServer::builder()
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_echo_round_trips_request_header_and_payload() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("ECHO:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // Handshake: send VERSION, drain server's VERSION reply.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
        .await
        .expect("server VERSION reply timed out")
        .expect("read VERSION");

    // Send CA_PROTO_ECHO with a non-trivial header AND an 8-byte
    // payload — the C server is documented to echo m_postsize bytes
    // verbatim.
    let mut echo = CaHeader::new(CA_PROTO_ECHO);
    echo.data_type = 0xAAAA;
    echo.count = 0; // padded post-write — set_payload_size below will adjust
    echo.cid = 0x1122_3344;
    echo.available = 0xAABB_CCDD;
    echo.set_payload_size(8, 0);
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

/// R2-24: C `event_cancel_reply` (`rsrv/camessage.c:1998-2021`)
/// calls `MPTOPCIU(mp)` first. If the request's channel id is
/// unknown or belongs to another client, rsrv calls `logBadId` and
/// returns RSRV_ERROR WITHOUT sending a wire error frame. Only
/// after a valid channel resolves does rsrv walk that channel's
/// event queue and emit ECA_BADMONID for an unknown monitor id.
///
/// Pre-fix Rust checked the flat subscription map first, so an
/// unknown SID elicited ECA_BADMONID via the diagnostic fallback
/// path. This test now asserts the silent-close behaviour for the
/// bad-SID case (matches C `logBadId`); the valid-SID + bad-sub_id
/// case is covered by `server_event_cancel_bad_subid_on_valid_sid_replies_eca_badmonid`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_event_cancel_unknown_sid_closes_silently() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("BADMONID:PV", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect a raw TCP socket and complete the CA handshake.
    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // Send VERSION (priority=0, minor=13).
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();

    // Send HOST_NAME and CLIENT_NAME (server will ack VERSION but the
    // handshake doesn't strictly require these — keep minimal).
    // Server replies with its own VERSION; we drain it before
    // proceeding.
    let mut buf = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
        .await
        .expect("server VERSION reply timed out")
        .expect("read VERSION");

    // Send EVENT_CANCEL with an SID that was never opened. Per R2-24
    // server must close the connection without emitting a wire frame
    // (C `event_cancel_reply` MPTOPCIU → logBadId silent path).
    let mut cancel = CaHeader::new(CA_PROTO_EVENT_CANCEL);
    cancel.data_type = 6; // DBR_DOUBLE
    cancel.count = 1;
    cancel.cid = 0xDEAD_BEEF; // bogus sid
    cancel.available = 0xCAFE_BABE; // bogus sub_id
    sock.write_all(&cancel.to_bytes()).await.unwrap();

    // Expect silent EOF — no wire frame, just connection drop.
    let mut resp = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut resp))
        .await
        .expect("server did not close after EVENT_CANCEL bad-SID")
        .expect("read after bad-SID cancel");
    assert_eq!(
        n, 0,
        "EVENT_CANCEL with unknown SID must elicit a silent close \
         (matches C event_cancel_reply logBadId); got {n} bytes: {:02x?}",
        &resp[..n]
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_unknown_tcp_command_replies_error_and_disconnects() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("BAD:CMD", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // Handshake: send VERSION, drain server's VERSION reply.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf))
        .await
        .expect("server VERSION reply timed out")
        .expect("read VERSION");

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_tcp_version_below_minimum_drops_connection() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("VER:OLD", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // CA V4.0 (minor = 0) is below CA_MINIMUM_SUPPORTED_VERSION = 4.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = 0;
    sock.write_all(&ver.to_bytes()).await.unwrap();

    // Server must drop the connection — no VERSION reply, just EOF.
    let mut buf = [0u8; 64];
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_write_notify_bad_type_replies_error_and_disconnects() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("WRBAD:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

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
        h.set_payload_size(body.len(), 0);
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
    create.set_payload_size(create_body.len(), 0);
    let mut frame = Vec::new();
    frame.extend_from_slice(&create.to_bytes());
    frame.extend_from_slice(&create_body);
    sock.write_all(&frame).await.unwrap();

    // Drain ACCESS_RIGHTS + CREATE_CHAN reply (read up to 64 bytes)
    let mut buf = [0u8; 128];
    let _ = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await;
    // Find the CREATE_CHAN reply to extract SID. ACCESS_RIGHTS comes
    // first (16 bytes); CREATE_CHAN reply follows. Parse second header.
    let create_resp = CaHeader::from_bytes(&buf[16..32]).expect("parse CREATE_CHAN");
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

/// C `read_notify_action` (`rsrv/camessage.c:693-697`): `INVALID_DB_REQ`
/// (data_type > LAST_BUFFER_TYPE = 38) returns RSRV_ERROR WITHOUT
/// emitting any wire frame — only the deprecated `read_action`
/// (`camessage.c:616-620`) calls `send_err(ECA_BADTYPE)` here.
/// R2-6: pre-fix Rust sent a CA_PROTO_READ_NOTIFY error frame for
/// the notify path too, an extra wire frame before EOF that rsrv
/// never produces. Test asserts the silent-close behaviour: no
/// wire frame, just connection drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_read_notify_bad_type_closes_silently() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("RDBAD:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    // VERSION + CLIENT_NAME + HOST_NAME + CREATE_CHAN handshake.
    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut hello = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello))
        .await
        .expect("VERSION reply timed out")
        .expect("read VERSION");
    for (cmd, name) in [
        (CA_PROTO_CLIENT_NAME, "testuser\0"),
        (CA_PROTO_HOST_NAME, "testhost\0"),
    ] {
        let mut h = CaHeader::new(cmd);
        let mut body = name.as_bytes().to_vec();
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }
        h.set_payload_size(body.len(), 0);
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
    create.set_payload_size(create_body.len(), 0);
    let mut frame = Vec::new();
    frame.extend_from_slice(&create.to_bytes());
    frame.extend_from_slice(&create_body);
    sock.write_all(&frame).await.unwrap();
    let mut buf = [0u8; 128];
    let _ = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await;
    let create_resp = CaHeader::from_bytes(&buf[16..32]).expect("parse CREATE_CHAN");
    let sid = create_resp.available;

    // READ_NOTIFY with data_type = 200 (well past LAST_BUFFER_TYPE = 38).
    let mut bad = CaHeader::new(CA_PROTO_READ_NOTIFY);
    bad.data_type = 200;
    bad.count = 1;
    bad.cid = sid;
    bad.available = 0xFADE_FADE; // ioid
    sock.write_all(&bad.to_bytes()).await.unwrap();

    // R2-6: server must drop the connection WITHOUT emitting a
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn server_read_sync_echoes_request_header() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let port = {
        let probe =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA server port");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };

    let server = CaServer::builder()
        .port(port)
        .pv("SYNC:PV", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("build CA server");
    let _rs_handle = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    let mut ver = CaHeader::new(CA_PROTO_VERSION);
    ver.count = CA_MINOR_VERSION;
    sock.write_all(&ver.to_bytes()).await.unwrap();
    let mut hello = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut hello))
        .await
        .expect("VERSION reply timed out")
        .expect("read VERSION");

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
