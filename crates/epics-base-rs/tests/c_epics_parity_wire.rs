//! Tests: CA wire protocol, .db parsing, autosave format.
//!
//! Source files:
//!   - CA protocol: modules/ca/src/client/test/ca_test.c
//!   - DB loader: modules/database/test/ioc/db/dbStaticTest.c
//!   - autosave: modules/database/test/std/rec/asTestLib.c

use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::protocol::{
    CA_PROTO_READ_NOTIFY, CA_PROTO_RSRV_IS_UP, CA_PROTO_SEARCH, CA_PROTO_VERSION,
    CA_PROTO_WRITE_NOTIFY, CaHeader, align8, pad_string,
};

// ============================================================
// CA Wire Protocol — Header Encoding/Decoding
// ============================================================

#[test]
fn ca_header_roundtrip_standard() {
    let mut hdr = CaHeader::new(CA_PROTO_VERSION);
    hdr.count = 13;
    let bytes = hdr.to_bytes();
    assert_eq!(bytes.len(), 16);
    let parsed = CaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_VERSION);
    assert_eq!(parsed.count, 13);
}

#[test]
fn ca_header_search_message() {
    let mut hdr = CaHeader::new(CA_PROTO_SEARCH);
    hdr.data_type = 5;
    hdr.count = 13;
    hdr.cid = 42;
    hdr.available = 42;
    let bytes = hdr.to_bytes();
    let parsed = CaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_SEARCH);
    assert_eq!(parsed.cid, 42);
    assert_eq!(parsed.available, 42);
}

#[test]
fn ca_header_read_notify() {
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
    hdr.data_type = 6;
    hdr.count = 1;
    hdr.cid = 100;
    hdr.available = 200;
    let bytes = hdr.to_bytes();
    let parsed = CaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_READ_NOTIFY);
    assert_eq!(parsed.data_type, 6);
    assert_eq!(parsed.cid, 100);
    assert_eq!(parsed.available, 200);
}

#[test]
fn ca_header_write_notify() {
    let mut hdr = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    hdr.cid = 55;
    hdr.available = 77;
    let bytes = hdr.to_bytes();
    let parsed = CaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_WRITE_NOTIFY);
    assert_eq!(parsed.cid, 55);
    assert_eq!(parsed.available, 77);
}

#[test]
fn ca_header_extended_large_payload() {
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
    hdr.data_type = 6;
    hdr.set_payload_size(100_000, 10_000);
    let bytes = hdr.to_bytes_extended();
    assert!(bytes.len() > 16);
    let (parsed, _consumed) = CaHeader::from_bytes_extended(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_READ_NOTIFY);
    assert_eq!(parsed.actual_postsize(), 100_000);
    assert_eq!(parsed.actual_count(), 10_000);
}

#[test]
fn ca_header_beacon() {
    let mut hdr = CaHeader::new(CA_PROTO_RSRV_IS_UP);
    hdr.data_type = 5064;
    let bytes = hdr.to_bytes();
    let parsed = CaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.cmmd, CA_PROTO_RSRV_IS_UP);
    assert_eq!(parsed.data_type, 5064);
}

#[test]
fn ca_string_padding() {
    let padded = pad_string("TEST");
    assert_eq!(padded.len() % 8, 0);
    assert!(padded.len() >= 5);
    assert_eq!(padded[4], 0);
}

#[test]
fn ca_align8_values() {
    assert_eq!(align8(0), 0);
    assert_eq!(align8(1), 8);
    assert_eq!(align8(7), 8);
    assert_eq!(align8(8), 8);
    assert_eq!(align8(9), 16);
    assert_eq!(align8(16), 16);
}

// ============================================================
// DBR Value Encoding/Decoding Roundtrip
// ============================================================

#[test]
fn dbr_all_native_types_roundtrip() {
    let test_cases: Vec<(DbFieldType, EpicsValue)> = vec![
        (DbFieldType::String, EpicsValue::String("test".into())),
        (DbFieldType::Short, EpicsValue::Short(42)),
        (DbFieldType::Float, EpicsValue::Float(2.5)),
        (DbFieldType::Enum, EpicsValue::Enum(3)),
        (DbFieldType::Char, EpicsValue::Char(65)),
        (DbFieldType::Long, EpicsValue::Long(100000)),
        (DbFieldType::Double, EpicsValue::Double(99.99)),
    ];

    for (dbf, val) in test_cases {
        let bytes = val.to_bytes();
        let back = EpicsValue::from_bytes(dbf, &bytes).unwrap();
        assert_eq!(back, val, "Roundtrip failed for {dbf:?}");
    }
}

#[test]
fn dbr_double_array_roundtrip() {
    let val = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    let bytes = val.to_bytes();
    let back = EpicsValue::from_bytes_array(DbFieldType::Double, &bytes, 5).unwrap();
    assert_eq!(back, val);
}

#[test]
fn dbr_long_array_roundtrip() {
    let val = EpicsValue::LongArray(vec![10, 20, 30, -40, 50]);
    let bytes = val.to_bytes();
    let back = EpicsValue::from_bytes_array(DbFieldType::Long, &bytes, 5).unwrap();
    assert_eq!(back, val);
}

#[test]
fn dbr_boundary_values() {
    for v in [i32::MAX, i32::MIN, 0] {
        let val = EpicsValue::Long(v);
        let bytes = val.to_bytes();
        let back = EpicsValue::from_bytes(DbFieldType::Long, &bytes).unwrap();
        assert_eq!(back, val, "Long boundary failed for {v}");
    }
    for v in [i16::MAX, i16::MIN, 0] {
        let val = EpicsValue::Short(v);
        let bytes = val.to_bytes();
        let back = EpicsValue::from_bytes(DbFieldType::Short, &bytes).unwrap();
        assert_eq!(back, val, "Short boundary failed for {v}");
    }
}

#[test]
fn dbr_empty_string() {
    let val = EpicsValue::String(String::new());
    let bytes = val.to_bytes();
    let back = EpicsValue::from_bytes(DbFieldType::String, &bytes).unwrap();
    assert_eq!(back, val);
}

#[test]
fn dbr_max_string() {
    let val = EpicsValue::String("A".repeat(39));
    let bytes = val.to_bytes();
    let back = EpicsValue::from_bytes(DbFieldType::String, &bytes).unwrap();
    assert_eq!(back, val);
}

// ============================================================
// .db File Parsing
// ============================================================

#[test]
fn db_parse_simple_record() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(ai, "TEST:temp") {
    field(VAL, "25.0")
    field(EGU, "degC")
    field(PREC, "2")
}
"#;

    let records = parse_db(db_content, &std::collections::HashMap::new()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "TEST:temp");
    assert_eq!(records[0].record_type, "ai");

    let val = records[0].fields.iter().find(|(k, _)| k == "VAL").unwrap();
    assert_eq!(val.1, "25.0");
    let egu = records[0].fields.iter().find(|(k, _)| k == "EGU").unwrap();
    assert_eq!(egu.1, "degC");
}

#[test]
fn db_parse_multiple_records() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(ai, "PV1") { field(VAL, "1.0") }
record(ao, "PV2") { field(VAL, "2.0") }
record(bi, "PV3") { field(VAL, "0") }
"#;

    let records = parse_db(db_content, &std::collections::HashMap::new()).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].name, "PV1");
    assert_eq!(records[1].name, "PV2");
    assert_eq!(records[2].name, "PV3");
}

#[test]
fn db_parse_macro_substitution() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(ao, "$(P)$(R)") {
    field(VAL, "$(VAL=0.0)")
    field(EGU, "$(EGU=mm)")
}
"#;

    let mut macros = std::collections::HashMap::new();
    macros.insert("P".to_string(), "IOC:".to_string());
    macros.insert("R".to_string(), "temp".to_string());

    let records = parse_db(db_content, &macros).unwrap();
    assert_eq!(records[0].name, "IOC:temp");
    let val = records[0].fields.iter().find(|(k, _)| k == "VAL").unwrap();
    assert_eq!(val.1, "0.0"); // default
    let egu = records[0].fields.iter().find(|(k, _)| k == "EGU").unwrap();
    assert_eq!(egu.1, "mm"); // default
}

#[test]
fn db_parse_macro_override() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(ao, "$(P)rec") {
    field(EGU, "$(EGU=mm)")
}
"#;

    let mut macros = std::collections::HashMap::new();
    macros.insert("P".to_string(), "TEST:".to_string());
    macros.insert("EGU".to_string(), "degC".to_string());

    let records = parse_db(db_content, &macros).unwrap();
    assert_eq!(records[0].name, "TEST:rec");
    let egu = records[0].fields.iter().find(|(k, _)| k == "EGU").unwrap();
    assert_eq!(egu.1, "degC");
}

#[test]
fn db_parse_comments() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
# This is a comment
record(ai, "commented") {
    # field comment
    field(VAL, "1.0")
}
"#;

    let records = parse_db(db_content, &std::collections::HashMap::new()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "commented");
}

#[test]
fn db_parse_motor_template() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(motor, "$(P)$(M)") {
    field(DTYP, "simMotor_$(PORT)")
    field(SCAN, "I/O Intr")
    field(VELO, "$(VELO=1.0)")
    field(ACCL, "$(ACCL=0.5)")
    field(MRES, "$(MRES=0.001)")
}
"#;

    let mut macros = std::collections::HashMap::new();
    macros.insert("P".to_string(), "SIM:".to_string());
    macros.insert("M".to_string(), "mtr1".to_string());
    macros.insert("PORT".to_string(), "motor1".to_string());

    let records = parse_db(db_content, &macros).unwrap();
    assert_eq!(records[0].name, "SIM:mtr1");
    let dtyp = records[0].fields.iter().find(|(k, _)| k == "DTYP").unwrap();
    assert_eq!(dtyp.1, "simMotor_motor1");
    let velo = records[0].fields.iter().find(|(k, _)| k == "VELO").unwrap();
    assert_eq!(velo.1, "1.0");
}

#[test]
fn db_parse_empty_input() {
    use epics_base_rs::server::db_loader::parse_db;

    let records = parse_db("", &std::collections::HashMap::new()).unwrap();
    assert!(records.is_empty());

    let records = parse_db("# only comments\n", &std::collections::HashMap::new()).unwrap();
    assert!(records.is_empty());
}

#[test]
fn db_parse_nested_macro_default() {
    use epics_base_rs::server::db_loader::parse_db;

    let db_content = r#"
record(ao, "$(P=$(DEFAULT))rec") {
    field(VAL, "0")
}
"#;

    let mut macros = std::collections::HashMap::new();
    macros.insert("DEFAULT".to_string(), "FALLBACK:".to_string());

    let records = parse_db(db_content, &macros).unwrap();
    // P not set, should use default which expands $(DEFAULT) → FALLBACK:
    assert_eq!(records[0].name, "FALLBACK:rec");
}

// ============================================================
// Autosave .sav File Format
// ============================================================

#[test]
fn autosave_value_to_save_str() {
    use epics_base_rs::server::autosave::save_file::value_to_save_str;

    // C autosave uses scientific notation for doubles
    let s = value_to_save_str(&EpicsValue::Double(42.5));
    let parsed: f64 = s.parse().unwrap();
    assert!(
        (parsed - 42.5).abs() < 1e-10,
        "Double roundtrip: {s} -> {parsed}"
    );
    assert_eq!(value_to_save_str(&EpicsValue::Long(100)), "100");
    let s = value_to_save_str(&EpicsValue::String("hello".into()));
    assert!(
        s.contains("hello"),
        "String save format should contain 'hello': {s}"
    );
}

#[test]
fn autosave_parse_save_value_double() {
    use epics_base_rs::server::autosave::save_file::parse_save_value;

    let template = EpicsValue::Double(0.0);
    let result = parse_save_value("42.5", &template);
    assert_eq!(result, Some(EpicsValue::Double(42.5)));
}

#[test]
fn autosave_parse_save_value_long() {
    use epics_base_rs::server::autosave::save_file::parse_save_value;

    let template = EpicsValue::Long(0);
    let result = parse_save_value("100", &template);
    assert_eq!(result, Some(EpicsValue::Long(100)));
}

#[test]
fn autosave_parse_save_value_string() {
    use epics_base_rs::server::autosave::save_file::parse_save_value;

    let template = EpicsValue::String(String::new());
    let result = parse_save_value("hello world", &template);
    assert_eq!(result, Some(EpicsValue::String("hello world".into())));
}

#[test]
fn autosave_request_file_parsing() {
    use epics_base_rs::server::autosave::parse_request_file;

    let content = "# comment\nIOC:temp\nIOC:status\n# another comment\nIOC:name\n";
    let pvs = parse_request_file(content);
    assert_eq!(pvs.len(), 3);
    assert!(pvs.contains(&"IOC:temp".to_string()));
    assert!(pvs.contains(&"IOC:status".to_string()));
    assert!(pvs.contains(&"IOC:name".to_string()));
}

#[test]
fn autosave_macro_expansion() {
    use epics_base_rs::server::autosave::macros::MacroContext;

    let mut map = std::collections::HashMap::new();
    map.insert("P".to_string(), "IOC:".to_string());
    map.insert("R".to_string(), "temp".to_string());
    let ctx = MacroContext::from_map(map);

    let result = ctx.expand("$(P)$(R)", "test", 1).unwrap();
    assert_eq!(result, "IOC:temp");
}

#[test]
fn autosave_macro_default_value() {
    use epics_base_rs::server::autosave::macros::MacroContext;

    let ctx = MacroContext::new(); // empty macros
    let result = ctx.expand("$(P=DEFAULT:)rec", "test", 1).unwrap();
    assert_eq!(result, "DEFAULT:rec");
}
