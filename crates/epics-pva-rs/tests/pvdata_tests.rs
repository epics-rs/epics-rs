//! Integration tests for epics-pva-rs: PVData structures, serialization, and codec.

use epics_pva_rs::protocol::*;
use epics_pva_rs::pvdata::*;
use epics_pva_rs::serialize::*;

// ---------------------------------------------------------------------------
// ScalarType type_code / from_type_code roundtrip
// ---------------------------------------------------------------------------

#[test]
fn scalar_type_code_roundtrip() {
    let types = [
        ScalarType::Boolean,
        ScalarType::Byte,
        ScalarType::Short,
        ScalarType::Int,
        ScalarType::Long,
        ScalarType::UByte,
        ScalarType::UShort,
        ScalarType::UInt,
        ScalarType::ULong,
        ScalarType::Float,
        ScalarType::Double,
        ScalarType::String,
    ];
    for st in types {
        let code = st.type_code();
        let decoded = ScalarType::from_type_code(code)
            .unwrap_or_else(|| panic!("from_type_code failed for {st}"));
        assert_eq!(st, decoded, "roundtrip mismatch for {st}");
    }
}

#[test]
fn scalar_type_array_code_roundtrip() {
    let types = [
        ScalarType::Boolean,
        ScalarType::Byte,
        ScalarType::Short,
        ScalarType::Int,
        ScalarType::Long,
        ScalarType::UByte,
        ScalarType::UShort,
        ScalarType::UInt,
        ScalarType::ULong,
        ScalarType::Float,
        ScalarType::Double,
        ScalarType::String,
    ];
    for st in types {
        let arr_code = st.array_type_code();
        // Array code should have bit 3 set
        assert_ne!(arr_code & 0x08, 0, "array bit not set for {st}");
        let decoded = ScalarType::from_array_type_code(arr_code)
            .unwrap_or_else(|| panic!("from_array_type_code failed for {st}"));
        assert_eq!(st, decoded);
    }
}

#[test]
fn scalar_type_from_invalid_code_returns_none() {
    assert!(ScalarType::from_type_code(0x99).is_none());
    assert!(ScalarType::from_type_code(0x10).is_none());
    // A non-array code should not decode as array
    assert!(ScalarType::from_array_type_code(ScalarType::Double.type_code()).is_none());
}

// ---------------------------------------------------------------------------
// ScalarValue creation and scalar_type()
// ---------------------------------------------------------------------------

#[test]
fn scalar_value_type_detection() {
    assert_eq!(
        ScalarValue::Boolean(true).scalar_type(),
        ScalarType::Boolean
    );
    assert_eq!(ScalarValue::Byte(1).scalar_type(), ScalarType::Byte);
    assert_eq!(ScalarValue::Short(2).scalar_type(), ScalarType::Short);
    assert_eq!(ScalarValue::Int(3).scalar_type(), ScalarType::Int);
    assert_eq!(ScalarValue::Long(4).scalar_type(), ScalarType::Long);
    assert_eq!(ScalarValue::UByte(5).scalar_type(), ScalarType::UByte);
    assert_eq!(ScalarValue::UShort(6).scalar_type(), ScalarType::UShort);
    assert_eq!(ScalarValue::UInt(7).scalar_type(), ScalarType::UInt);
    assert_eq!(ScalarValue::ULong(8).scalar_type(), ScalarType::ULong);
    assert_eq!(ScalarValue::Float(9.0).scalar_type(), ScalarType::Float);
    assert_eq!(ScalarValue::Double(10.0).scalar_type(), ScalarType::Double);
    assert_eq!(
        ScalarValue::String("x".into()).scalar_type(),
        ScalarType::String
    );
}

// ---------------------------------------------------------------------------
// ScalarValue::parse
// ---------------------------------------------------------------------------

#[test]
fn scalar_value_parse_all_types() {
    assert_eq!(
        ScalarValue::parse(ScalarType::Boolean, "true").unwrap(),
        ScalarValue::Boolean(true)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Boolean, "0").unwrap(),
        ScalarValue::Boolean(false)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Byte, "-5").unwrap(),
        ScalarValue::Byte(-5)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Short, "1000").unwrap(),
        ScalarValue::Short(1000)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Int, "-999999").unwrap(),
        ScalarValue::Int(-999999)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Long, "9999999999").unwrap(),
        ScalarValue::Long(9999999999)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::UByte, "255").unwrap(),
        ScalarValue::UByte(255)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::UShort, "65535").unwrap(),
        ScalarValue::UShort(65535)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::UInt, "4000000000").unwrap(),
        ScalarValue::UInt(4_000_000_000)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::ULong, "18446744073709551615").unwrap(),
        ScalarValue::ULong(u64::MAX)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Float, "3.15").unwrap(),
        ScalarValue::Float(3.15)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::Double, "2.72").unwrap(),
        ScalarValue::Double(2.72)
    );
    assert_eq!(
        ScalarValue::parse(ScalarType::String, "hello world").unwrap(),
        ScalarValue::String("hello world".into())
    );
}

#[test]
fn scalar_value_parse_invalid_boolean() {
    assert!(ScalarValue::parse(ScalarType::Boolean, "maybe").is_err());
}

#[test]
fn scalar_value_parse_invalid_number() {
    assert!(ScalarValue::parse(ScalarType::Int, "not_a_number").is_err());
}

// ---------------------------------------------------------------------------
// PvStructure creation and field access
// ---------------------------------------------------------------------------

#[test]
fn pv_structure_new_and_get_field() {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.0))));
    s.fields.push((
        "descriptor".into(),
        PvField::Scalar(ScalarValue::String("test".into())),
    ));

    assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");

    // get_field
    let val_field = s.get_field("value").unwrap();
    match val_field {
        PvField::Scalar(ScalarValue::Double(v)) => assert_eq!(*v, 42.0),
        _ => panic!("expected Scalar(Double)"),
    }

    let desc_field = s.get_field("descriptor").unwrap();
    match desc_field {
        PvField::Scalar(ScalarValue::String(v)) => assert_eq!(v, "test"),
        _ => panic!("expected Scalar(String)"),
    }

    // Nonexistent field
    assert!(s.get_field("nonexistent").is_none());
}

#[test]
fn pv_structure_get_value_helper() {
    let mut s = PvStructure::new("test_t");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Int(99))));

    let val = s.get_value().unwrap();
    assert_eq!(*val, ScalarValue::Int(99));
}

#[test]
fn pv_structure_get_alarm_and_timestamp() {
    let mut alarm = PvStructure::new("alarm_t");
    alarm
        .fields
        .push(("severity".into(), PvField::Scalar(ScalarValue::Int(0))));
    alarm
        .fields
        .push(("status".into(), PvField::Scalar(ScalarValue::Int(0))));

    let mut ts = PvStructure::new("time_t");
    ts.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(1000)),
    ));
    ts.fields
        .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(500))));

    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
    s.fields.push(("alarm".into(), PvField::Structure(alarm)));
    s.fields.push(("timeStamp".into(), PvField::Structure(ts)));

    let a = s.get_alarm().unwrap();
    assert_eq!(a.struct_id, "alarm_t");
    assert_eq!(a.fields.len(), 2);

    let t = s.get_timestamp().unwrap();
    assert_eq!(t.struct_id, "time_t");
    match t.get_field("secondsPastEpoch").unwrap() {
        PvField::Scalar(ScalarValue::Long(v)) => assert_eq!(*v, 1000),
        _ => panic!("expected Long"),
    }
}

#[test]
fn pv_structure_nested_structure() {
    let inner = PvStructure {
        struct_id: "inner_t".into(),
        fields: vec![("x".into(), PvField::Scalar(ScalarValue::Float(1.0)))],
    };
    let mut outer = PvStructure::new("outer_t");
    outer
        .fields
        .push(("nested".into(), PvField::Structure(inner)));

    match outer.get_field("nested").unwrap() {
        PvField::Structure(s) => {
            assert_eq!(s.struct_id, "inner_t");
            match s.get_field("x").unwrap() {
                PvField::Scalar(ScalarValue::Float(v)) => assert_eq!(*v, 1.0),
                _ => panic!("expected Float"),
            }
        }
        _ => panic!("expected Structure"),
    }
}

// ---------------------------------------------------------------------------
// PvField::ScalarArray
// ---------------------------------------------------------------------------

#[test]
fn pv_field_scalar_array() {
    let arr = PvField::ScalarArray(vec![
        ScalarValue::Int(1),
        ScalarValue::Int(2),
        ScalarValue::Int(3),
    ]);
    match &arr {
        PvField::ScalarArray(v) => {
            assert_eq!(v.len(), 3);
            assert_eq!(v[0], ScalarValue::Int(1));
            assert_eq!(v[2], ScalarValue::Int(3));
        }
        _ => panic!("expected ScalarArray"),
    }
}

// ---------------------------------------------------------------------------
// FieldDesc creation and helpers
// ---------------------------------------------------------------------------

#[test]
fn field_desc_value_scalar_type() {
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            (
                "alarm".into(),
                FieldDesc::Structure {
                    struct_id: "alarm_t".into(),
                    fields: vec![],
                },
            ),
        ],
    };
    assert_eq!(desc.value_scalar_type(), Some(ScalarType::Double));
}

#[test]
fn field_desc_value_scalar_type_none_for_non_structure() {
    let desc = FieldDesc::Scalar(ScalarType::Int);
    assert_eq!(desc.value_scalar_type(), None);
}

#[test]
fn field_desc_field_count() {
    let desc = FieldDesc::Structure {
        struct_id: "".into(),
        fields: vec![
            ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("c".into(), FieldDesc::ScalarArray(ScalarType::String)),
        ],
    };
    assert_eq!(desc.field_count(), 3);

    let scalar = FieldDesc::Scalar(ScalarType::Float);
    assert_eq!(scalar.field_count(), 0);
}

// ---------------------------------------------------------------------------
// Serialization: size encoding roundtrip
// ---------------------------------------------------------------------------

#[test]
fn size_encoding_roundtrip_both_endians() {
    for be in [false, true] {
        // Small value (single byte)
        for val in [0, 1, 100, 253] {
            let mut buf = Vec::new();
            write_size(&mut buf, val, be);
            let mut pos = 0;
            assert_eq!(read_size(&buf, &mut pos, be).unwrap(), val);
            assert_eq!(pos, buf.len());
        }

        // Large value (0xFE prefix + 4 bytes)
        for val in [254, 1000, 65536, 1_000_000] {
            let mut buf = Vec::new();
            write_size(&mut buf, val, be);
            assert_eq!(buf[0], 0xFE);
            let mut pos = 0;
            assert_eq!(read_size(&buf, &mut pos, be).unwrap(), val);
            assert_eq!(pos, buf.len());
        }

        // Null (-1)
        let mut buf = Vec::new();
        write_size(&mut buf, -1, be);
        assert_eq!(buf, vec![0xFF]);
        let mut pos = 0;
        assert_eq!(read_size(&buf, &mut pos, be).unwrap(), -1);
    }
}

// ---------------------------------------------------------------------------
// Serialization: string encoding roundtrip
// ---------------------------------------------------------------------------

#[test]
fn string_encoding_roundtrip_both_endians() {
    let test_strings = [
        "",
        "a",
        "hello world",
        "EPICS:PV:NAME",
        "unicode: \u{00e9}\u{00e8}",
    ];
    for be in [false, true] {
        for s in &test_strings {
            let mut buf = Vec::new();
            write_string(&mut buf, s, be);
            let mut pos = 0;
            let decoded = read_string(&buf, &mut pos, be).unwrap();
            assert_eq!(&decoded, s);
            assert_eq!(pos, buf.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization: scalar value roundtrip for all types
// ---------------------------------------------------------------------------

#[test]
fn scalar_value_serialization_roundtrip() {
    let values = [
        ScalarValue::Boolean(true),
        ScalarValue::Boolean(false),
        ScalarValue::Byte(-42),
        ScalarValue::Short(1234),
        ScalarValue::Int(-999_999),
        ScalarValue::Long(i64::MAX),
        ScalarValue::UByte(255),
        ScalarValue::UShort(65535),
        ScalarValue::UInt(u32::MAX),
        ScalarValue::ULong(u64::MAX),
        ScalarValue::Float(std::f32::consts::PI),
        ScalarValue::Double(std::f64::consts::E),
        ScalarValue::String("test string".into()),
        ScalarValue::String("".into()),
    ];

    for be in [false, true] {
        for val in &values {
            let mut buf = Vec::new();
            write_scalar_value(&mut buf, val, be);
            let mut pos = 0;
            let decoded = read_scalar_value(&buf, &mut pos, val.scalar_type(), be).unwrap();
            assert_eq!(&decoded, val, "mismatch for {val:?} (be={be})");
            assert_eq!(pos, buf.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization: FieldDesc roundtrip
// ---------------------------------------------------------------------------

#[test]
fn field_desc_serialization_roundtrip_scalar() {
    for be in [false, true] {
        let desc = FieldDesc::Scalar(ScalarType::Double);
        let mut buf = Vec::new();
        write_field_desc(&mut buf, &desc, be);
        let mut pos = 0;
        let decoded = read_field_desc(&buf, &mut pos, be).unwrap();
        match decoded {
            FieldDesc::Scalar(st) => assert_eq!(st, ScalarType::Double),
            _ => panic!("expected Scalar"),
        }
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn field_desc_serialization_roundtrip_scalar_array() {
    for be in [false, true] {
        let desc = FieldDesc::ScalarArray(ScalarType::Int);
        let mut buf = Vec::new();
        write_field_desc(&mut buf, &desc, be);
        let mut pos = 0;
        let decoded = read_field_desc(&buf, &mut pos, be).unwrap();
        match decoded {
            FieldDesc::ScalarArray(st) => assert_eq!(st, ScalarType::Int),
            _ => panic!("expected ScalarArray"),
        }
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn field_desc_serialization_roundtrip_nested_structure() {
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            (
                "alarm".into(),
                FieldDesc::Structure {
                    struct_id: "alarm_t".into(),
                    fields: vec![
                        ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                    ],
                },
            ),
            (
                "timeStamp".into(),
                FieldDesc::Structure {
                    struct_id: "time_t".into(),
                    fields: vec![
                        (
                            "secondsPastEpoch".into(),
                            FieldDesc::Scalar(ScalarType::Long),
                        ),
                        ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                    ],
                },
            ),
            (
                "display".into(),
                FieldDesc::Structure {
                    struct_id: "display_t".into(),
                    fields: vec![
                        ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
                        ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
                        ("description".into(), FieldDesc::Scalar(ScalarType::String)),
                        ("units".into(), FieldDesc::Scalar(ScalarType::String)),
                    ],
                },
            ),
        ],
    };

    for be in [false, true] {
        let mut buf = Vec::new();
        write_field_desc(&mut buf, &desc, be);
        let mut pos = 0;
        let decoded = read_field_desc(&buf, &mut pos, be).unwrap();
        assert_eq!(pos, buf.len());

        match &decoded {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                assert_eq!(fields.len(), 4);
                assert_eq!(fields[0].0, "value");
                assert_eq!(fields[1].0, "alarm");
                assert_eq!(fields[2].0, "timeStamp");
                assert_eq!(fields[3].0, "display");

                // Verify nested alarm structure
                match &fields[1].1 {
                    FieldDesc::Structure { struct_id, fields } => {
                        assert_eq!(struct_id, "alarm_t");
                        assert_eq!(fields.len(), 3);
                    }
                    _ => panic!("expected alarm Structure"),
                }
            }
            _ => panic!("expected Structure"),
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization: PvField roundtrip
// ---------------------------------------------------------------------------

#[test]
fn pv_field_scalar_serialization_roundtrip() {
    let desc = FieldDesc::Scalar(ScalarType::Double);
    let field = PvField::Scalar(ScalarValue::Double(3.14160));

    for be in [false, true] {
        let mut buf = Vec::new();
        write_pv_field(&mut buf, &field, be);
        let mut pos = 0;
        let decoded = read_pv_field(&buf, &mut pos, &desc, be).unwrap();
        match decoded {
            PvField::Scalar(ScalarValue::Double(v)) => {
                assert!((v - 3.14160).abs() < 1e-10);
            }
            _ => panic!("expected Scalar(Double)"),
        }
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn pv_field_scalar_array_serialization_roundtrip() {
    let desc = FieldDesc::ScalarArray(ScalarType::Int);
    let field = PvField::ScalarArray(vec![
        ScalarValue::Int(10),
        ScalarValue::Int(20),
        ScalarValue::Int(30),
    ]);

    for be in [false, true] {
        let mut buf = Vec::new();
        write_pv_field(&mut buf, &field, be);
        let mut pos = 0;
        let decoded = read_pv_field(&buf, &mut pos, &desc, be).unwrap();
        match decoded {
            PvField::ScalarArray(arr) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], ScalarValue::Int(10));
                assert_eq!(arr[1], ScalarValue::Int(20));
                assert_eq!(arr[2], ScalarValue::Int(30));
            }
            _ => panic!("expected ScalarArray"),
        }
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn pv_field_structure_serialization_roundtrip() {
    let desc = FieldDesc::Structure {
        struct_id: "test_t".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("name".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    };
    let field = PvField::Structure(PvStructure {
        struct_id: "test_t".into(),
        fields: vec![
            ("value".into(), PvField::Scalar(ScalarValue::Double(99.9))),
            (
                "name".into(),
                PvField::Scalar(ScalarValue::String("sensor".into())),
            ),
        ],
    });

    for be in [false, true] {
        let mut buf = Vec::new();
        write_pv_field(&mut buf, &field, be);
        let mut pos = 0;
        let decoded = read_pv_field(&buf, &mut pos, &desc, be).unwrap();
        match decoded {
            PvField::Structure(s) => {
                assert_eq!(s.struct_id, "test_t");
                assert_eq!(s.fields.len(), 2);
                match s.get_value().unwrap() {
                    ScalarValue::Double(v) => assert!((v - 99.9).abs() < 1e-10),
                    _ => panic!("expected Double value"),
                }
                match s.get_field("name").unwrap() {
                    PvField::Scalar(ScalarValue::String(v)) => assert_eq!(v, "sensor"),
                    _ => panic!("expected String"),
                }
            }
            _ => panic!("expected Structure"),
        }
        assert_eq!(pos, buf.len());
    }
}

// ---------------------------------------------------------------------------
// Serialization: bitset encode/decode
// ---------------------------------------------------------------------------

#[test]
fn bitset_roundtrip() {
    let bits = vec![0b10101010, 0b01010101];
    for be in [false, true] {
        let mut buf = Vec::new();
        write_bitset(&mut buf, &bits, be);
        let mut pos = 0;
        let decoded = read_bitset(&buf, &mut pos, be).unwrap();
        assert_eq!(decoded, bits);
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn bitset_get_checks() {
    let bits = vec![0b00010011]; // bits 0, 1, 4
    assert!(bitset_get(&bits, 0));
    assert!(bitset_get(&bits, 1));
    assert!(!bitset_get(&bits, 2));
    assert!(!bitset_get(&bits, 3));
    assert!(bitset_get(&bits, 4));
    assert!(!bitset_get(&bits, 5));
    // Out of range returns false
    assert!(!bitset_get(&bits, 100));
}

#[test]
fn bitset_empty() {
    let bits: Vec<u8> = vec![];
    for be in [false, true] {
        let mut buf = Vec::new();
        write_bitset(&mut buf, &bits, be);
        let mut pos = 0;
        let decoded = read_bitset(&buf, &mut pos, be).unwrap();
        assert!(decoded.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Serialization: primitive read/write roundtrip
// ---------------------------------------------------------------------------

#[test]
fn primitive_u8_roundtrip() {
    let mut buf = Vec::new();
    write_u8(&mut buf, 0xAB);
    let mut pos = 0;
    assert_eq!(read_u8(&buf, &mut pos).unwrap(), 0xAB);
}

#[test]
fn primitive_u16_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_u16(&mut buf, 0x1234, be);
        let mut pos = 0;
        assert_eq!(read_u16(&buf, &mut pos, be).unwrap(), 0x1234);
    }
}

#[test]
fn primitive_u32_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_u32(&mut buf, 0xDEADBEEF, be);
        let mut pos = 0;
        assert_eq!(read_u32(&buf, &mut pos, be).unwrap(), 0xDEADBEEF);
    }
}

#[test]
fn primitive_i32_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_i32(&mut buf, -123456, be);
        let mut pos = 0;
        assert_eq!(read_i32(&buf, &mut pos, be).unwrap(), -123456);
    }
}

#[test]
fn primitive_i64_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_i64(&mut buf, i64::MIN, be);
        let mut pos = 0;
        assert_eq!(read_i64(&buf, &mut pos, be).unwrap(), i64::MIN);
    }
}

#[test]
fn primitive_f32_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_f32(&mut buf, std::f32::consts::PI, be);
        let mut pos = 0;
        assert_eq!(read_f32(&buf, &mut pos, be).unwrap(), std::f32::consts::PI);
    }
}

#[test]
fn primitive_f64_roundtrip() {
    for be in [false, true] {
        let mut buf = Vec::new();
        write_f64(&mut buf, std::f64::consts::E, be);
        let mut pos = 0;
        assert_eq!(read_f64(&buf, &mut pos, be).unwrap(), std::f64::consts::E);
    }
}

// ---------------------------------------------------------------------------
// Serialization: error on truncated buffers
// ---------------------------------------------------------------------------

#[test]
fn read_u16_from_short_buffer_fails() {
    let buf = [0x01]; // need 2 bytes
    let mut pos = 0;
    assert!(read_u16(&buf, &mut pos, false).is_err());
}

#[test]
fn read_u32_from_short_buffer_fails() {
    let buf = [0x01, 0x02]; // need 4 bytes
    let mut pos = 0;
    assert!(read_u32(&buf, &mut pos, false).is_err());
}

#[test]
fn read_i64_from_short_buffer_fails() {
    let buf = [0u8; 4]; // need 8 bytes
    let mut pos = 0;
    assert!(read_i64(&buf, &mut pos, false).is_err());
}

#[test]
fn read_string_truncated_fails() {
    // Write a string header saying length=100, but only provide 5 data bytes
    let mut buf = Vec::new();
    write_size(&mut buf, 100, false);
    buf.extend_from_slice(b"short");
    let mut pos = 0;
    assert!(read_string(&buf, &mut pos, false).is_err());
}

// ---------------------------------------------------------------------------
// PVA protocol header roundtrip
// ---------------------------------------------------------------------------

#[test]
fn pva_header_roundtrip_le() {
    let hdr = PvaHeader {
        magic: PVA_MAGIC,
        version: PVA_VERSION,
        flags: FLAGS_APP_NONSEG_LE,
        command: CMD_GET,
        payload_size: 4096,
    };
    let bytes = hdr.to_bytes(false);
    assert_eq!(bytes.len(), PvaHeader::SIZE);
    let decoded = PvaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.magic, PVA_MAGIC);
    assert_eq!(decoded.version, PVA_VERSION);
    assert_eq!(decoded.flags, FLAGS_APP_NONSEG_LE);
    assert_eq!(decoded.command, CMD_GET);
    assert_eq!(decoded.payload_size, 4096);
    assert!(!decoded.is_big_endian());
    assert!(!decoded.is_control());
}

#[test]
fn pva_header_roundtrip_be() {
    let hdr = PvaHeader {
        magic: PVA_MAGIC,
        version: PVA_VERSION,
        flags: FLAGS_APP_NONSEG_BE,
        command: CMD_PUT,
        payload_size: 0x12345678,
    };
    let bytes = hdr.to_bytes(true);
    let decoded = PvaHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.payload_size, 0x12345678);
    assert!(decoded.is_big_endian());
}

#[test]
fn pva_header_bad_magic_fails() {
    let mut bytes = PvaHeader::new(CMD_ECHO, FLAGS_APP_NONSEG_LE).to_bytes(false);
    bytes[0] = 0xFF; // corrupt magic
    assert!(PvaHeader::from_bytes(&bytes).is_err());
}

#[test]
fn pva_header_control_flag() {
    let hdr = PvaHeader {
        magic: PVA_MAGIC,
        version: PVA_VERSION,
        flags: FLAG_CONTROL | FLAG_BIG_ENDIAN,
        command: CMD_SET_BYTE_ORDER,
        payload_size: 0,
    };
    let bytes = hdr.to_bytes(true);
    let decoded = PvaHeader::from_bytes(&bytes).unwrap();
    assert!(decoded.is_control());
    assert!(decoded.is_big_endian());
}

#[test]
fn pva_header_from_short_buffer_fails() {
    let buf = [PVA_MAGIC, PVA_VERSION, 0, 0]; // only 4 bytes
    assert!(PvaHeader::from_bytes(&buf).is_err());
}

// ---------------------------------------------------------------------------
// Codec: build_message wraps payload correctly
// ---------------------------------------------------------------------------

#[test]
fn codec_build_message_structure() {
    use epics_pva_rs::codec::PvaCodec;

    let codec = PvaCodec::new();
    let payload = b"test payload";
    let msg = codec.build_message(CMD_ECHO, payload);

    // Should be header (8 bytes) + payload
    assert_eq!(msg.len(), PvaHeader::SIZE + payload.len());

    // Parse header
    let hdr = PvaHeader::from_bytes(&msg).unwrap();
    assert_eq!(hdr.magic, PVA_MAGIC);
    assert_eq!(hdr.command, CMD_ECHO);
    assert_eq!(hdr.payload_size, payload.len() as u32);

    // Payload follows header
    assert_eq!(&msg[PvaHeader::SIZE..], payload);
}

// ---------------------------------------------------------------------------
// Serialization: status encoding
// ---------------------------------------------------------------------------

#[test]
fn status_ok_roundtrip() {
    let mut buf = Vec::new();
    write_status_ok(&mut buf);
    assert_eq!(buf, vec![0xFF]);
    let mut pos = 0;
    assert!(read_status(&buf, &mut pos, false).is_ok());
}

// ---------------------------------------------------------------------------
// Display implementations
// ---------------------------------------------------------------------------

#[test]
fn scalar_type_display() {
    assert_eq!(format!("{}", ScalarType::Boolean), "boolean");
    assert_eq!(format!("{}", ScalarType::Double), "double");
    assert_eq!(format!("{}", ScalarType::String), "string");
}

#[test]
fn scalar_value_display() {
    assert_eq!(format!("{}", ScalarValue::Boolean(true)), "true");
    assert_eq!(format!("{}", ScalarValue::Int(42)), "42");
    assert_eq!(format!("{}", ScalarValue::String("hello".into())), "hello");
}

#[test]
fn pv_structure_display_with_value() {
    let mut s = PvStructure::new("nt_scalar");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.15))));
    // Display should show the value directly
    let display = format!("{s}");
    assert!(display.contains("3.15"));
}

#[test]
fn pv_field_scalar_array_display() {
    let field = PvField::ScalarArray(vec![
        ScalarValue::Int(1),
        ScalarValue::Int(2),
        ScalarValue::Int(3),
    ]);
    let display = format!("{field}");
    assert_eq!(display, "[1, 2, 3]");
}
