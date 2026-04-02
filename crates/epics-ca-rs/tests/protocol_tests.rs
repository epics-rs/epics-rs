//! Integration tests for epics-ca-rs: protocol encoding/decoding and server API.

use std::collections::HashMap;

use epics_ca_rs::EpicsValue;
use epics_ca_rs::protocol::*;
use epics_ca_rs::server::CaServer;

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

    // count = 0xFFFF fits in normal form
    hdr.set_payload_size(100, 0xFFFF);
    assert!(!hdr.is_extended());

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
    server.add_pv("RUNTIME:PV", EpicsValue::Double(42.0)).await;

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
