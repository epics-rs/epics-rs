#![allow(unused_imports, clippy::all)]
use epics_base_rs::server::snapshot::*;
use epics_base_rs::types::*;
use std::time::{Duration, SystemTime};

const EPICS_UNIX_EPOCH_OFFSET_SECS: u64 = 631_152_000;

#[test]
fn test_double_roundtrip() {
    let val = EpicsValue::Double(3.15);
    let bytes = val.to_bytes();
    let val2 = EpicsValue::from_bytes(DbFieldType::Double, &bytes).unwrap();
    match val2 {
        EpicsValue::Double(v) => assert!((v - 3.15).abs() < 1e-10),
        _ => panic!("wrong type"),
    }
}

#[test]
fn test_string_roundtrip() {
    let val = EpicsValue::String("hello".into());
    let bytes = val.to_bytes();
    assert_eq!(bytes.len(), 40);
    let val2 = EpicsValue::from_bytes(DbFieldType::String, &bytes).unwrap();
    match val2 {
        EpicsValue::String(s) => assert_eq!(s, "hello"),
        _ => panic!("wrong type"),
    }
}

#[test]
fn test_parse_values() {
    match EpicsValue::parse(DbFieldType::Long, "42").unwrap() {
        EpicsValue::Long(v) => assert_eq!(v, 42),
        _ => panic!("wrong type"),
    }
}

#[test]
fn test_serialize_ctrl_double_layout() {
    let val = EpicsValue::Double(42.0);
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 100);
    let data = serialize_dbr(34, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 88);
    assert_eq!(&data[8..16], &[0u8; 8]);
    assert_eq!(&data[80..88], &42.0f64.to_be_bytes());
}

#[test]
fn test_serialize_ctrl_long_layout() {
    let val = EpicsValue::Long(99);
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(33, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 48);
    assert_eq!(&data[4..12], &[0u8; 8]);
    assert_eq!(&data[44..48], &99i32.to_be_bytes());
}

#[test]
fn test_serialize_gr_short_layout() {
    let val = EpicsValue::Short(7);
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(22, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 26);
    assert_eq!(&data[24..26], &7i16.to_be_bytes());
}

#[test]
fn test_serialize_ctrl_enum_layout() {
    let val = EpicsValue::Enum(3);
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(31, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 424);
    assert_eq!(&data[422..424], &3u16.to_be_bytes());
}

#[test]
fn test_serialize_ctrl_char_layout() {
    let val = EpicsValue::Char(0xAB);
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(32, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 22);
    assert_eq!(data[21], 0xAB);
}

#[test]
fn test_serialize_ctrl_float_layout() {
    let val = EpicsValue::Float(1.5);
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(30, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 52);
    assert_eq!(&data[48..52], &1.5f32.to_be_bytes());
}

#[test]
fn test_serialize_gr_string_falls_back_to_sts() {
    let val = EpicsValue::String("test".into());
    let ts = SystemTime::UNIX_EPOCH;
    let data = serialize_dbr(21, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 44);
}

// ---- Golden packet tests ----

#[test]
fn test_golden_plain_string() {
    let val = EpicsValue::String("hello".into());
    let data = serialize_dbr(0, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data.len(), 40);
    assert_eq!(&data[..5], b"hello");
    assert_eq!(&data[5..], &[0u8; 35]);
}

#[test]
fn test_golden_plain_short() {
    let val = EpicsValue::Short(42);
    let data = serialize_dbr(1, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, 42i16.to_be_bytes());
}

#[test]
fn test_golden_plain_float() {
    let val = EpicsValue::Float(1.5);
    let data = serialize_dbr(2, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, 1.5f32.to_be_bytes());
}

#[test]
fn test_golden_plain_enum() {
    let val = EpicsValue::Enum(7);
    let data = serialize_dbr(3, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, 7u16.to_be_bytes());
}

#[test]
fn test_golden_plain_char() {
    let val = EpicsValue::Char(0xFF);
    let data = serialize_dbr(4, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, [0xFF]);
}

#[test]
fn test_golden_plain_long() {
    let val = EpicsValue::Long(-1000);
    let data = serialize_dbr(5, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, (-1000i32).to_be_bytes());
}

#[test]
fn test_golden_plain_double() {
    let val = EpicsValue::Double(std::f64::consts::PI);
    let data = serialize_dbr(6, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data, std::f64::consts::PI.to_be_bytes());
}

#[test]
fn test_golden_sts_double() {
    let val = EpicsValue::Double(99.9);
    let data = serialize_dbr(13, &val, 3, 2, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data.len(), 14);
    assert_eq!(&data[0..2], &3u16.to_be_bytes());
    assert_eq!(&data[2..4], &2u16.to_be_bytes());
    assert_eq!(&data[4..6], &[0, 0]);
    assert_eq!(&data[6..14], &99.9f64.to_be_bytes());
}

#[test]
fn test_golden_sts_char() {
    let val = EpicsValue::Char(0x42);
    let data = serialize_dbr(11, &val, 1, 1, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data.len(), 6);
    assert_eq!(&data[0..2], &1u16.to_be_bytes());
    assert_eq!(&data[2..4], &1u16.to_be_bytes());
    assert_eq!(data[4], 0);
    assert_eq!(data[5], 0x42);
}

#[test]
fn test_golden_time_double() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1000);
    let val = EpicsValue::Double(1.23);
    let data = serialize_dbr(20, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 24);
    assert_eq!(&data[0..2], &0u16.to_be_bytes());
    assert_eq!(&data[2..4], &0u16.to_be_bytes());
    assert_eq!(&data[4..8], &1000u32.to_be_bytes());
    assert_eq!(&data[8..12], &0u32.to_be_bytes());
    assert_eq!(&data[12..16], &[0, 0, 0, 0]);
    assert_eq!(&data[16..24], &1.23f64.to_be_bytes());
}

#[test]
fn test_golden_time_short() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 500);
    let val = EpicsValue::Short(777);
    let data = serialize_dbr(15, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[12..14], &[0, 0]);
    assert_eq!(&data[14..16], &777i16.to_be_bytes());
}

#[test]
fn test_golden_time_char() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
    let val = EpicsValue::Char(0xBE);
    let data = serialize_dbr(18, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[12..15], &[0, 0, 0]);
    assert_eq!(data[15], 0xBE);
}

#[test]
fn test_golden_time_float() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
    let val = EpicsValue::Float(2.5);
    let data = serialize_dbr(16, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[12..16], &2.5f32.to_be_bytes());
}

#[test]
fn test_golden_time_enum() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1);
    let val = EpicsValue::Enum(5);
    let data = serialize_dbr(17, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[12..14], &[0, 0]);
    assert_eq!(&data[14..16], &5u16.to_be_bytes());
}

#[test]
fn test_golden_time_string() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 99);
    let val = EpicsValue::String("abc".into());
    let data = serialize_dbr(14, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 52);
    assert_eq!(&data[12..15], b"abc");
    assert_eq!(&data[15..52], &[0u8; 37]);
}

#[test]
fn test_golden_gr_matches_time() {
    let val = EpicsValue::Double(42.0);
    let ts = SystemTime::UNIX_EPOCH;
    let gr = serialize_dbr(27, &val, 0, 0, ts).unwrap();
    let time = serialize_dbr(20, &val, 0, 0, ts).unwrap();
    assert_eq!(gr.len(), 72);
    assert_eq!(time.len(), 24);
    assert_ne!(gr, time);
    assert_eq!(&gr[4..64], &[0u8; 60]);
}

#[test]
fn test_golden_ctrl_matches_gr_pattern() {
    let val = EpicsValue::Double(42.0);
    let ts = SystemTime::UNIX_EPOCH;
    let ctrl = serialize_dbr(34, &val, 0, 0, ts).unwrap();
    let gr = serialize_dbr(27, &val, 0, 0, ts).unwrap();
    assert_eq!(ctrl.len(), gr.len() + 16);
    assert_eq!(&ctrl[0..4], &gr[0..4]);
    assert_eq!(&ctrl[4..64], &[0u8; 60]);
}

#[test]
fn test_golden_type_conversion() {
    let val = EpicsValue::Double(42.7);
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
    let data = serialize_dbr(15, &val, 0, 0, ts).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[14..16], &42i16.to_be_bytes());
}

#[test]
fn test_golden_header_read_notify() {
    use epics_ca_rs::protocol::*;
    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
    hdr.data_type = 20;
    hdr.set_payload_size(24, 1);
    hdr.cid = ECA_NORMAL;
    hdr.available = 42;

    let bytes = hdr.to_bytes_extended();
    assert_eq!(&bytes[0..2], &CA_PROTO_READ_NOTIFY.to_be_bytes());
    assert_eq!(&bytes[4..6], &20u16.to_be_bytes());
    assert_eq!(&bytes[12..16], &42u32.to_be_bytes());
}

// ---- encode_dbr tests ----

fn bare_snapshot(value: EpicsValue) -> Snapshot {
    Snapshot::new(value, 0, 0, SystemTime::UNIX_EPOCH)
}

fn full_snapshot(value: EpicsValue) -> Snapshot {
    let mut snap = Snapshot::new(value, 3, 2, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "degC".to_string(),
        precision: 3,
        upper_disp_limit: 100.0,
        lower_disp_limit: -50.0,
        upper_alarm_limit: 90.0,
        upper_warning_limit: 80.0,
        lower_warning_limit: -20.0,
        lower_alarm_limit: -40.0,
    });
    snap.control = Some(ControlInfo {
        upper_ctrl_limit: 95.0,
        lower_ctrl_limit: -45.0,
    });
    snap
}

#[test]
fn test_encode_plain_matches_serialize() {
    let val = EpicsValue::Double(42.0);
    let ts = SystemTime::UNIX_EPOCH;
    let snap = bare_snapshot(val.clone());
    assert_eq!(
        encode_dbr(6, &snap).unwrap(),
        serialize_dbr(6, &val, 0, 0, ts).unwrap()
    );
}

#[test]
fn test_encode_sts_matches_serialize() {
    let val = EpicsValue::Short(77);
    let ts = SystemTime::UNIX_EPOCH;
    let mut snap = bare_snapshot(val.clone());
    snap.alarm = AlarmInfo {
        status: 5,
        severity: 1,
    };
    assert_eq!(
        encode_dbr(8, &snap).unwrap(),
        serialize_dbr(8, &val, 5, 1, ts).unwrap()
    );
}

#[test]
fn test_encode_time_matches_serialize() {
    let val = EpicsValue::Double(1.23);
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 500);
    let mut snap = bare_snapshot(val.clone());
    snap.timestamp = ts;
    snap.alarm = AlarmInfo {
        status: 1,
        severity: 2,
    };
    assert_eq!(
        encode_dbr(20, &snap).unwrap(),
        serialize_dbr(20, &val, 1, 2, ts).unwrap()
    );
}

#[test]
fn test_encode_gr_double_with_metadata() {
    let snap = full_snapshot(EpicsValue::Double(42.0));
    let data = encode_dbr(27, &snap).unwrap();
    assert_eq!(data.len(), 72);
    assert_eq!(&data[0..2], &3u16.to_be_bytes());
    assert_eq!(&data[2..4], &2u16.to_be_bytes());
    assert_eq!(&data[4..6], &3i16.to_be_bytes());
    assert_eq!(&data[6..8], &[0, 0]);
    assert_eq!(&data[8..12], b"degC");
    assert_eq!(&data[12..16], &[0, 0, 0, 0]);
    assert_eq!(&data[16..24], &100.0f64.to_be_bytes());
    assert_eq!(&data[24..32], &(-50.0f64).to_be_bytes());
    assert_eq!(&data[32..40], &90.0f64.to_be_bytes());
    assert_eq!(&data[40..48], &80.0f64.to_be_bytes());
    assert_eq!(&data[48..56], &(-20.0f64).to_be_bytes());
    assert_eq!(&data[56..64], &(-40.0f64).to_be_bytes());
    assert_eq!(&data[64..72], &42.0f64.to_be_bytes());
}

#[test]
fn test_encode_ctrl_double_with_metadata() {
    let snap = full_snapshot(EpicsValue::Double(42.0));
    let data = encode_dbr(34, &snap).unwrap();
    assert_eq!(data.len(), 88);
    assert_eq!(&data[64..72], &95.0f64.to_be_bytes());
    assert_eq!(&data[72..80], &(-45.0f64).to_be_bytes());
    assert_eq!(&data[80..88], &42.0f64.to_be_bytes());
}

#[test]
fn test_encode_gr_short_with_metadata() {
    let mut snap = Snapshot::new(EpicsValue::Short(42), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "mm".to_string(),
        precision: 0,
        upper_disp_limit: 1000.0,
        lower_disp_limit: -100.0,
        upper_alarm_limit: 900.0,
        upper_warning_limit: 800.0,
        lower_warning_limit: -50.0,
        lower_alarm_limit: -90.0,
    });
    let data = encode_dbr(22, &snap).unwrap();
    assert_eq!(data.len(), 26);
    assert_eq!(&data[4..6], b"mm");
    assert_eq!(&data[12..14], &1000i16.to_be_bytes());
    assert_eq!(&data[24..26], &42i16.to_be_bytes());
}

#[test]
fn test_encode_gr_float_with_metadata() {
    let mut snap = Snapshot::new(EpicsValue::Float(1.5), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "V".to_string(),
        precision: 2,
        upper_disp_limit: 10.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    let data = encode_dbr(23, &snap).unwrap();
    assert_eq!(data.len(), 44);
    assert_eq!(&data[4..6], &2i16.to_be_bytes());
    assert_eq!(data[8], b'V');
    assert_eq!(&data[16..20], &10.0f32.to_be_bytes());
}

#[test]
fn test_encode_gr_long_with_metadata() {
    let mut snap = Snapshot::new(EpicsValue::Long(99), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "cnt".to_string(),
        upper_disp_limit: 10000.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    let data = encode_dbr(26, &snap).unwrap();
    assert_eq!(data.len(), 40);
    assert_eq!(&data[12..16], &10000i32.to_be_bytes());
    assert_eq!(&data[36..40], &99i32.to_be_bytes());
}

#[test]
fn test_encode_gr_char_with_metadata() {
    let mut snap = Snapshot::new(EpicsValue::Char(42), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "raw".to_string(),
        upper_disp_limit: 255.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    let data = encode_dbr(25, &snap).unwrap();
    assert_eq!(data.len(), 20);
    assert_eq!(data[12], 255);
    assert_eq!(data[13], 0);
    assert_eq!(data[19], 42);
}

#[test]
fn test_encode_gr_enum_with_strings() {
    let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, SystemTime::UNIX_EPOCH);
    snap.enums = Some(EnumInfo {
        strings: vec!["Off".to_string(), "On".to_string()],
    });
    let data = encode_dbr(24, &snap).unwrap();
    assert_eq!(data.len(), 424);
    assert_eq!(&data[4..6], &2u16.to_be_bytes());
    assert_eq!(&data[6..9], b"Off");
    assert_eq!(data[9], 0);
    assert_eq!(&data[32..34], b"On");
    assert_eq!(&data[422..424], &1u16.to_be_bytes());
}

#[test]
fn test_encode_gr_none_metadata_matches_serialize() {
    let val = EpicsValue::Double(42.0);
    let snap = bare_snapshot(val.clone());
    let encoded = encode_dbr(27, &snap).unwrap();
    let legacy = serialize_dbr(27, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(encoded, legacy);
}

#[test]
fn test_encode_ctrl_none_metadata_matches_serialize() {
    let val = EpicsValue::Long(99);
    let snap = bare_snapshot(val.clone());
    let encoded = encode_dbr(33, &snap).unwrap();
    let legacy = serialize_dbr(33, &val, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(encoded, legacy);
}

#[test]
fn test_encode_ctrl_short_with_ctrl_limits() {
    let mut snap = Snapshot::new(EpicsValue::Short(10), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "mA".to_string(),
        upper_disp_limit: 100.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    snap.control = Some(ControlInfo {
        upper_ctrl_limit: 80.0,
        lower_ctrl_limit: 5.0,
    });
    let data = encode_dbr(29, &snap).unwrap();
    assert_eq!(data.len(), 30);
    assert_eq!(&data[12..14], &100i16.to_be_bytes());
    assert_eq!(&data[24..26], &80i16.to_be_bytes());
    assert_eq!(&data[26..28], &5i16.to_be_bytes());
    assert_eq!(&data[28..30], &10i16.to_be_bytes());
}

#[test]
fn test_encode_invalid_type() {
    let snap = bare_snapshot(EpicsValue::Double(0.0));
    assert!(encode_dbr(35, &snap).is_err());
    assert!(encode_dbr(100, &snap).is_err());
}

#[test]
fn test_parse_menu_string_alarm_sevr() {
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "NO_ALARM").unwrap(),
        EpicsValue::Short(0)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "MINOR").unwrap(),
        EpicsValue::Short(1)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "MAJOR").unwrap(),
        EpicsValue::Short(2)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "INVALID").unwrap(),
        EpicsValue::Short(3)
    );
}

#[test]
fn test_parse_menu_string_omsl() {
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "supervisory").unwrap(),
        EpicsValue::Short(0)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "closed_loop").unwrap(),
        EpicsValue::Short(1)
    );
}

#[test]
fn test_parse_menu_string_enum_type() {
    assert_eq!(
        EpicsValue::parse(DbFieldType::Enum, "NO_ALARM").unwrap(),
        EpicsValue::Enum(0)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Enum, "MAJOR").unwrap(),
        EpicsValue::Enum(2)
    );
}

#[test]
fn test_parse_menu_string_numeric_priority() {
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "0").unwrap(),
        EpicsValue::Short(0)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "42").unwrap(),
        EpicsValue::Short(42)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Enum, "3").unwrap(),
        EpicsValue::Enum(3)
    );
}

#[test]
fn test_parse_menu_string_unknown() {
    assert!(EpicsValue::parse(DbFieldType::Short, "UNKNOWN_MENU").is_err());
    assert!(EpicsValue::parse(DbFieldType::Enum, "UNKNOWN_MENU").is_err());
}

// ---- decode_dbr roundtrip tests ----

#[test]
fn test_decode_plain_double() {
    let data = 42.0f64.to_be_bytes();
    let snap = decode_dbr(6, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(42.0));
    assert_eq!(snap.alarm.status, 0);
}

#[test]
fn test_decode_sts_double_roundtrip() {
    let val = EpicsValue::Double(99.9);
    let data = serialize_dbr(13, &val, 3, 2, SystemTime::UNIX_EPOCH).unwrap();
    let snap = decode_dbr(13, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(99.9));
    assert_eq!(snap.alarm.status, 3);
    assert_eq!(snap.alarm.severity, 2);
}

#[test]
fn test_decode_time_double_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1000);
    let val = EpicsValue::Double(1.23);
    let data = serialize_dbr(20, &val, 5, 1, ts).unwrap();
    let snap = decode_dbr(20, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(1.23));
    assert_eq!(snap.alarm.status, 5);
    assert_eq!(snap.alarm.severity, 1);
    let orig_secs = ts.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    let decoded_secs = snap
        .timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(orig_secs, decoded_secs);
}

#[test]
fn test_decode_time_short_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 500);
    let val = EpicsValue::Short(777);
    let data = serialize_dbr(15, &val, 0, 0, ts).unwrap();
    let snap = decode_dbr(15, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Short(777));
}

#[test]
fn test_decode_time_char_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 10);
    let val = EpicsValue::Char(0xBE);
    let data = serialize_dbr(18, &val, 0, 0, ts).unwrap();
    let snap = decode_dbr(18, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Char(0xBE));
}

#[test]
fn test_decode_time_float_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
    let val = EpicsValue::Float(2.5);
    let data = serialize_dbr(16, &val, 0, 0, ts).unwrap();
    let snap = decode_dbr(16, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Float(2.5));
}

#[test]
fn test_decode_time_enum_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 1);
    let val = EpicsValue::Enum(5);
    let data = serialize_dbr(17, &val, 0, 0, ts).unwrap();
    let snap = decode_dbr(17, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(5));
}

#[test]
fn test_decode_time_string_roundtrip() {
    let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + 99);
    let val = EpicsValue::String("abc".into());
    let data = serialize_dbr(14, &val, 0, 0, ts).unwrap();
    let snap = decode_dbr(14, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::String("abc".into()));
}

#[test]
fn test_decode_ctrl_double_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Double(42.0));
    let data = encode_dbr(34, &snap_orig).unwrap();
    let snap = decode_dbr(34, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(42.0));
    assert_eq!(snap.alarm.status, 3);
    assert_eq!(snap.alarm.severity, 2);
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.precision, 3);
    assert_eq!(disp.upper_disp_limit, 100.0);
    assert_eq!(disp.lower_disp_limit, -50.0);
    assert_eq!(disp.upper_alarm_limit, 90.0);
    assert_eq!(disp.upper_warning_limit, 80.0);
    assert_eq!(disp.lower_warning_limit, -20.0);
    assert_eq!(disp.lower_alarm_limit, -40.0);
    let ctrl = snap.control.unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 95.0);
    assert_eq!(ctrl.lower_ctrl_limit, -45.0);
}

#[test]
fn test_decode_ctrl_float_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Float(1.5));
    let data = encode_dbr(30, &snap_orig).unwrap();
    let snap = decode_dbr(30, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Float(1.5));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.precision, 3);
    assert!((disp.upper_disp_limit - 100.0).abs() < 0.01);
    let ctrl = snap.control.unwrap();
    assert!((ctrl.upper_ctrl_limit - 95.0).abs() < 0.01);
}

#[test]
fn test_decode_ctrl_long_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Long(99));
    let data = encode_dbr(33, &snap_orig).unwrap();
    let snap = decode_dbr(33, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Long(99));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.upper_disp_limit, 100.0);
    assert_eq!(disp.lower_disp_limit, -50.0);
    let ctrl = snap.control.unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 95.0);
    assert_eq!(ctrl.lower_ctrl_limit, -45.0);
}

#[test]
fn test_decode_ctrl_short_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Short(7));
    let data = encode_dbr(29, &snap_orig).unwrap();
    let snap = decode_dbr(29, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Short(7));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
}

#[test]
fn test_decode_ctrl_char_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Char(0xAB));
    let data = encode_dbr(32, &snap_orig).unwrap();
    let snap = decode_dbr(32, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Char(0xAB));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
}

#[test]
fn test_decode_ctrl_enum_roundtrip() {
    let mut snap_orig = full_snapshot(EpicsValue::Enum(2));
    snap_orig.enums = Some(EnumInfo {
        strings: vec!["Off".into(), "On".into(), "Reset".into()],
    });
    let data = encode_dbr(31, &snap_orig).unwrap();
    let snap = decode_dbr(31, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(2));
    let ei = snap.enums.unwrap();
    assert_eq!(ei.strings.len(), 3);
    assert_eq!(ei.strings[0], "Off");
    assert_eq!(ei.strings[1], "On");
    assert_eq!(ei.strings[2], "Reset");
}

#[test]
fn test_decode_gr_double_roundtrip() {
    let snap_orig = full_snapshot(EpicsValue::Double(3.15));
    let data = encode_dbr(27, &snap_orig).unwrap();
    let snap = decode_dbr(27, &data, 1).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(3.15));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.precision, 3);
    assert_eq!(disp.upper_disp_limit, 100.0);
    assert!(snap.control.is_none());
}

#[test]
fn test_dbr_type_helpers() {
    assert_eq!(DbFieldType::Double.time_dbr_type(), 20);
    assert_eq!(DbFieldType::Short.time_dbr_type(), 15);
    assert_eq!(DbFieldType::Double.ctrl_dbr_type(), 34);
    assert_eq!(DbFieldType::Long.ctrl_dbr_type(), 33);
    assert_eq!(DbFieldType::String.time_dbr_type(), 14);
    assert_eq!(DbFieldType::Char.ctrl_dbr_type(), 32);
}
