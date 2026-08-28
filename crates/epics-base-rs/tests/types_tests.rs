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
    // C `struct dbr_sts_double` (db_access.h): status(2) + severity(2) +
    // RISC_pad(dbr_long_t = epicsInt32, 4 bytes) + value(8) = 16 bytes,
    // value at offset 8. (Parity fix: the pad is 4 bytes, not 2.)
    let val = EpicsValue::Double(99.9);
    let data = serialize_dbr(13, &val, 3, 2, SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!(data.len(), 16);
    assert_eq!(&data[0..2], &3u16.to_be_bytes());
    assert_eq!(&data[2..4], &2u16.to_be_bytes());
    assert_eq!(&data[4..8], &[0, 0, 0, 0]);
    assert_eq!(&data[8..16], &99.9f64.to_be_bytes());
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
    // precision(2) + RISC_pad(2) + units[8] + the two display limits are
    // zero — but the four alarm limits are NOT. C's `get_alarm` seeds them
    // `epicsNAN` and copies them into the reply whether or not the record
    // supplied any (`dbAccess.c:294`, `:318-323`), where `get_graphics`
    // `memset`s its group for a missing slot (`:231`).
    assert_eq!(&gr[4..32], &[0u8; 28]);
    for i in 0..4 {
        let off = 32 + i * 8;
        assert!(
            f64::from_be_bytes(gr[off..off + 8].try_into().unwrap()).is_nan(),
            "alarm limit {i} of a metadata-less GR_DOUBLE must be nan"
        );
    }
}

#[test]
fn test_golden_ctrl_matches_gr_pattern() {
    let val = EpicsValue::Double(42.0);
    let ts = SystemTime::UNIX_EPOCH;
    let ctrl = serialize_dbr(34, &val, 0, 0, ts).unwrap();
    let gr = serialize_dbr(27, &val, 0, 0, ts).unwrap();
    assert_eq!(ctrl.len(), gr.len() + 16);
    assert_eq!(&ctrl[0..4], &gr[0..4]);
    assert_eq!(&ctrl[4..32], &[0u8; 28]);
    for i in 0..4 {
        let off = 32 + i * 8;
        assert!(
            f64::from_be_bytes(ctrl[off..off + 8].try_into().unwrap()).is_nan(),
            "alarm limit {i} of a metadata-less CTRL_DOUBLE must be nan"
        );
    }
    // The control group takes the same memset-zero seed as the display one
    // (`dbAccess.c:270`).
    assert_eq!(&ctrl[64..80], &[0u8; 16]);
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
    hdr.set_payload_size(24, 1, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
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

/// The rset-slot mask a channel carrying this much metadata supplies. The
/// encoder reads the MASK, not `display.is_some()`: a `DisplayInfo` is minted
/// for every snapshot to carry the DESC leaf, so its `Option` says nothing
/// about which `get_*` slots the record type has.
fn supplies_every_slot(snap: &mut Snapshot) {
    snap.properties = PropertySupport::NUMERIC.narrowed_to_field(snap.value.db_field_type(), false);
}

fn full_snapshot(value: EpicsValue) -> Snapshot {
    let mut snap = Snapshot::new(value, 3, 2, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "degC".into(),
        precision: 3,
        upper_disp_limit: 100.0,
        lower_disp_limit: -50.0,
        upper_alarm_limit: 90.0,
        upper_warning_limit: 80.0,
        lower_warning_limit: -20.0,
        lower_alarm_limit: -40.0,
        ..Default::default()
    });
    snap.control = Some(ControlInfo {
        upper_ctrl_limit: 95.0,
        lower_ctrl_limit: -45.0,
    });
    supplies_every_slot(&mut snap);
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
        ..Default::default()
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
    snap.timestamp = ts.into();
    snap.alarm = AlarmInfo {
        status: 1,
        severity: 2,
        ..Default::default()
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
        units: "mm".into(),
        precision: 0,
        upper_disp_limit: 1000.0,
        lower_disp_limit: -100.0,
        upper_alarm_limit: 900.0,
        upper_warning_limit: 800.0,
        lower_warning_limit: -50.0,
        lower_alarm_limit: -90.0,
        ..Default::default()
    });
    supplies_every_slot(&mut snap);
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
        units: "V".into(),
        precision: 2,
        upper_disp_limit: 10.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    supplies_every_slot(&mut snap);
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
        units: "cnt".into(),
        upper_disp_limit: 10000.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    supplies_every_slot(&mut snap);
    let data = encode_dbr(26, &snap).unwrap();
    assert_eq!(data.len(), 40);
    assert_eq!(&data[12..16], &10000i32.to_be_bytes());
    assert_eq!(&data[36..40], &99i32.to_be_bytes());
}

#[test]
fn test_encode_gr_char_with_metadata() {
    // DBF_CHAR limits are epicsInt8 (-128..127) per libca 7cb80d5a1.
    // Pre-fix: f64 → u8 saturated negatives to 0 and large positives
    // round-tripped fine but unsignedly. Post-fix: f64 → i8 → u8
    // bit-pattern preserves signed semantics. 100.0 stays 100;
    // -10.0 round-trips as 0xF6.
    let mut snap = Snapshot::new(EpicsValue::Char(42), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "raw".into(),
        upper_disp_limit: 100.0,
        lower_disp_limit: -10.0,
        ..Default::default()
    });
    supplies_every_slot(&mut snap);
    let data = encode_dbr(25, &snap).unwrap();
    assert_eq!(data.len(), 20);
    assert_eq!(data[12], 100u8); // 100 fits i8 → 100u8.
    assert_eq!(data[13], 0xF6u8); // -10i8 bit-pattern.
    assert_eq!(data[19], 42);
}

#[test]
fn test_encode_gr_char_truncates_out_of_range() {
    // C reaches a `dbr_char_t` limit through `epicsInt32`, and the second
    // step is an ordinary integer conversion: 255 keeps its byte, -200
    // keeps 0x38. Saturating straight from f64 would give 127 and 0x80,
    // neither of which C can put on the wire.
    let mut snap = Snapshot::new(EpicsValue::Char(0), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        upper_disp_limit: 255.0,
        lower_disp_limit: -200.0,
        ..Default::default()
    });
    supplies_every_slot(&mut snap);
    let data = encode_dbr(25, &snap).unwrap();
    assert_eq!(data[12], 0xFFu8); // 255 & 0xff
    assert_eq!(data[13], 0x38u8); // -200 & 0xff
}

#[test]
fn test_encode_gr_enum_with_strings() {
    let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, SystemTime::UNIX_EPOCH);
    snap.enums = Some(EnumInfo::new(vec!["Off".into(), "On".into()]));
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
        units: "mA".into(),
        upper_disp_limit: 100.0,
        lower_disp_limit: 0.0,
        ..Default::default()
    });
    snap.control = Some(ControlInfo {
        upper_ctrl_limit: 80.0,
        lower_ctrl_limit: 5.0,
    });
    supplies_every_slot(&mut snap);
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
    // 35..=38 are PUT_ACKT/PUT_ACKS/STSACK_STRING/CLASS_NAME (valid).
    // Anything beyond 38 is unallocated.
    assert!(encode_dbr(39, &snap).is_err());
    assert!(encode_dbr(100, &snap).is_err());
}

#[test]
fn test_encode_stsack_string_layout() {
    // STSACK_STRING wire layout: status(2) severity(2) ackt(2) acks(2) value(40) = 48 bytes
    let mut snap = bare_snapshot(EpicsValue::String("warn".into()));
    snap.alarm = AlarmInfo {
        status: 5,
        severity: 1,
        ackt: Some(1),
        acks: Some(2),
        amsg: String::new(),
    };
    let data = encode_dbr(epics_base_rs::types::DBR_STSACK_STRING, &snap).unwrap();
    assert_eq!(data.len(), 48);
    assert_eq!(&data[0..2], &5u16.to_be_bytes());
    assert_eq!(&data[2..4], &1u16.to_be_bytes());
    assert_eq!(&data[4..6], &1u16.to_be_bytes());
    assert_eq!(&data[6..8], &2u16.to_be_bytes());
    assert_eq!(&data[8..12], b"warn");
}

#[test]
fn test_encode_stsack_string_default_ackt_acks() {
    // ackt/acks default to None → encoded as zero so SimplePvs without
    // record-backed alarm fields still produce a valid response.
    let snap = bare_snapshot(EpicsValue::String("ok".into()));
    let data = encode_dbr(epics_base_rs::types::DBR_STSACK_STRING, &snap).unwrap();
    assert_eq!(data.len(), 48);
    assert_eq!(&data[4..6], &0u16.to_be_bytes());
    assert_eq!(&data[6..8], &0u16.to_be_bytes());
}

/// A menu label does NOT resolve through the field-blind
/// `EpicsValue::parse`, and must not: the same label names different
/// indices in different menus, so a table keyed on the label alone can
/// only guess which menu was meant.
///
/// This replaces three tests (`..._alarm_sevr`, `..._omsl`,
/// `..._enum_type`) that pinned that guess. They asserted
/// `parse(Short, "MINOR") == Short(1)` with no field in sight, which is
/// the defect 03 L-7 records, not a behaviour C has: C reaches every menu
/// label through `dbPutStringNum` with that field's own `pamenu`.
#[test]
fn parse_does_not_resolve_menu_labels_without_a_field() {
    for label in [
        "NO_ALARM",
        "MINOR",
        "MAJOR",
        "INVALID",
        "supervisory",
        "closed_loop",
    ] {
        assert!(
            EpicsValue::parse(DbFieldType::Short, label).is_err(),
            "{label} must not resolve to a DBF_SHORT index without its field"
        );
        assert!(
            EpicsValue::parse(DbFieldType::Enum, label).is_err(),
            "{label} must not resolve to a DBF_ENUM index without its field"
        );
    }

    // The sharp case: "Specified" is index 1 of `menuFanout` but index 0 of
    // `selSELM`. The old table answered 1 for both, so a `sel` record's
    // SELM took the wrong choice. No index at all is the correct answer
    // here — the field's own menu decides, elsewhere.
    assert!(EpicsValue::parse(DbFieldType::Enum, "Specified").is_err());

    // The error names the owner rather than just "invalid enum".
    let msg = format!(
        "{}",
        EpicsValue::parse(DbFieldType::Enum, "MINOR").unwrap_err()
    );
    assert!(msg.contains("menu label"), "unhelpful error: {msg}");

    // A numeric token is unaffected — this path still parses indices.
    assert_eq!(
        EpicsValue::parse(DbFieldType::Enum, "2").unwrap(),
        EpicsValue::Enum(2)
    );
    assert_eq!(
        EpicsValue::parse(DbFieldType::Short, "1").unwrap(),
        EpicsValue::Short(1)
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
    let decoded_secs = snap.timestamp.since_unix_epoch().as_secs();
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
    // Byte-identical round-trip, but not label-identical, and deliberately
    // so: the encoder takes the carrier the SERVER holds (a `DBF_CHAR`
    // field is `epicsInt8`), while the decoder answers with the carrier the
    // WIRE defines (`dbr_char_t` is `epicsUInt8`, `db_access.h:40`) —
    // `DbFieldType::wire_carrier`. Only a later widening can tell them
    // apart, and there the wire's answer is the one C's CA client gives.
    assert_eq!(snap.value, EpicsValue::UChar(0xBE));
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
    // Wire carrier on the way back, exactly as in the TIME_CHAR case above.
    assert_eq!(snap.value, EpicsValue::UChar(0xAB));
    let disp = snap.display.unwrap();
    assert_eq!(disp.units, "degC");
}

#[test]
fn test_decode_ctrl_enum_roundtrip() {
    let mut snap_orig = full_snapshot(EpicsValue::Enum(2));
    snap_orig.enums = Some(EnumInfo::new(vec![
        "Off".into(),
        "On".into(),
        "Reset".into(),
    ]));
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
    // CA-261/263: STS / GR helpers (added with per-variant constants)
    assert_eq!(DbFieldType::String.sts_dbr_type(), 7);
    assert_eq!(DbFieldType::Double.sts_dbr_type(), 13);
    assert_eq!(DbFieldType::String.gr_dbr_type(), 21);
    assert_eq!(DbFieldType::Double.gr_dbr_type(), 27);
    // Int64 has no CA wire type — collapses to Double in every layer
    assert_eq!(DbFieldType::Int64.sts_dbr_type(), 13);
    assert_eq!(DbFieldType::Int64.time_dbr_type(), 20);
    assert_eq!(DbFieldType::Int64.gr_dbr_type(), 27);
    assert_eq!(DbFieldType::Int64.ctrl_dbr_type(), 34);
}

#[test]
fn test_dbr_per_variant_constants_match_layer_arithmetic() {
    use epics_base_rs::types::{
        DBR_CHAR, DBR_CTRL_CHAR, DBR_CTRL_DOUBLE, DBR_CTRL_ENUM, DBR_CTRL_FLOAT, DBR_CTRL_LONG,
        DBR_CTRL_SHORT, DBR_CTRL_STRING, DBR_DOUBLE, DBR_ENUM, DBR_FLOAT, DBR_GR_CHAR,
        DBR_GR_DOUBLE, DBR_GR_ENUM, DBR_GR_FLOAT, DBR_GR_LONG, DBR_GR_SHORT, DBR_GR_STRING,
        DBR_INT, DBR_LONG, DBR_SHORT, DBR_STRING, DBR_STS_CHAR, DBR_STS_DOUBLE, DBR_STS_ENUM,
        DBR_STS_FLOAT, DBR_STS_LONG, DBR_STS_SHORT, DBR_STS_STRING,
    };
    // Native (CA-260)
    assert_eq!(DBR_STRING, 0);
    assert_eq!(DBR_DOUBLE, 6);
    // Aliases
    assert_eq!(DBR_INT, DBR_SHORT);
    // STS
    assert_eq!(DBR_STS_STRING, 7);
    assert_eq!(DBR_STS_SHORT, 8);
    assert_eq!(DBR_STS_FLOAT, 9);
    assert_eq!(DBR_STS_ENUM, 10);
    assert_eq!(DBR_STS_CHAR, 11);
    assert_eq!(DBR_STS_LONG, 12);
    assert_eq!(DBR_STS_DOUBLE, 13);
    // GR (CA-263) — main gap closure
    assert_eq!(DBR_GR_STRING, 21);
    assert_eq!(DBR_GR_SHORT, 22);
    assert_eq!(DBR_GR_FLOAT, 23);
    assert_eq!(DBR_GR_ENUM, 24);
    assert_eq!(DBR_GR_CHAR, 25);
    assert_eq!(DBR_GR_LONG, 26);
    assert_eq!(DBR_GR_DOUBLE, 27);
    // CTRL (CA-264) — main gap closure
    assert_eq!(DBR_CTRL_STRING, 28);
    assert_eq!(DBR_CTRL_SHORT, 29);
    assert_eq!(DBR_CTRL_FLOAT, 30);
    assert_eq!(DBR_CTRL_ENUM, 31);
    assert_eq!(DBR_CTRL_CHAR, 32);
    assert_eq!(DBR_CTRL_LONG, 33);
    assert_eq!(DBR_CTRL_DOUBLE, 34);
    // Round-trip: each per-variant constant matches the helper output
    for ft in [
        DbFieldType::String,
        DbFieldType::Short,
        DbFieldType::Float,
        DbFieldType::Enum,
        DbFieldType::Char,
        DbFieldType::Long,
        DbFieldType::Double,
    ] {
        assert_eq!(ft.sts_dbr_type(), ft as u16 + 7);
        assert_eq!(ft.time_dbr_type(), ft as u16 + 14);
        assert_eq!(ft.gr_dbr_type(), ft as u16 + 21);
        assert_eq!(ft.ctrl_dbr_type(), ft as u16 + 28);
    }
}

#[test]
fn test_dbr_class_name_round_trip() {
    use epics_base_rs::types::{DBR_CLASS_NAME, decode_dbr};
    let mut snap = bare_snapshot(EpicsValue::String(PvString::new()));
    snap.class_name = Some("ai".to_string());
    let data = encode_dbr(DBR_CLASS_NAME, &snap).unwrap();
    // Always 40 bytes regardless of the actual string length
    assert_eq!(data.len(), 40);
    // Wire form is null-padded "ai\0\0…"
    assert_eq!(&data[..2], b"ai");
    assert!(data[2..].iter().all(|&b| b == 0));

    let decoded = decode_dbr(DBR_CLASS_NAME, &data, 1).unwrap();
    assert_eq!(decoded.class_name.as_deref(), Some("ai"));
}

#[test]
fn test_dbr_class_name_truncates_long_record_type() {
    use epics_base_rs::types::DBR_CLASS_NAME;
    let mut snap = bare_snapshot(EpicsValue::String(PvString::new()));
    // 50 chars; 40-byte wire layout truncates to 39 + NUL
    snap.class_name = Some("a".repeat(50));
    let data = encode_dbr(DBR_CLASS_NAME, &snap).unwrap();
    assert_eq!(data.len(), 40);
    assert_eq!(&data[..39], &b"a".repeat(39)[..]);
    assert_eq!(data[39], 0); // last byte is NUL terminator
}

#[test]
fn test_dbr_class_name_empty_when_unpopulated() {
    use epics_base_rs::types::DBR_CLASS_NAME;
    let snap = bare_snapshot(EpicsValue::String(PvString::new()));
    // class_name is None — server emits an all-zero (empty) response,
    // matching what C IOC does for non-record-backed channels.
    let data = encode_dbr(DBR_CLASS_NAME, &snap).unwrap();
    assert_eq!(data.len(), 40);
    assert!(data.iter().all(|&b| b == 0));
}

// ── P-1 (libca 8cc20393f / a7bf59079): empty-array
// COUNT=0 round-trip. Pre-fix the `count <= 1` short-circuit raised
// CaError::Protocol("...too small") on GET and silently degraded
// array WRITE to a scalar PUT.

#[test]
fn p1_empty_array_double_decodes_as_empty_doublearray() {
    let v = EpicsValue::from_bytes_array(DbFieldType::Double, &[], 0).unwrap();
    assert_eq!(v, EpicsValue::DoubleArray(vec![]));
    assert_eq!(v.count(), 0);
}

#[test]
fn p1_count_one_double_falls_through_to_scalar() {
    // count == 1 still routes through the scalar decoder (the
    // legitimate "scalar shaped as array of one" CA case).
    let bytes = 7.5_f64.to_be_bytes();
    let v = EpicsValue::from_bytes_array(DbFieldType::Double, &bytes, 1).unwrap();
    assert_eq!(v, EpicsValue::Double(7.5));
}

#[test]
fn p1_empty_array_string_decodes_as_empty_stringarray() {
    // DBR_STRING uses 40-byte fixed-width slots — count=0 means no
    // bytes consumed; the decoder must produce StringArray(vec![])
    // not error on zero-length input.
    let v = EpicsValue::from_bytes_array(DbFieldType::String, &[], 0).unwrap();
    assert_eq!(v, EpicsValue::StringArray(vec![]));
    assert_eq!(v.count(), 0);
}

#[test]
fn p1_empty_array_all_dbr_types_round_trip() {
    // Every DBR type's count=0 path must produce its typed empty
    // variant. Catches future variants added without an arm in
    // from_bytes_array's count=0 dispatch.
    let cases: &[(DbFieldType, EpicsValue)] = &[
        (DbFieldType::Short, EpicsValue::ShortArray(vec![])),
        (DbFieldType::Float, EpicsValue::FloatArray(vec![])),
        (DbFieldType::Enum, EpicsValue::EnumArray(vec![])),
        (DbFieldType::Char, EpicsValue::CharArray(vec![])),
        (DbFieldType::Long, EpicsValue::LongArray(vec![])),
        (DbFieldType::Double, EpicsValue::DoubleArray(vec![])),
        (DbFieldType::String, EpicsValue::StringArray(vec![])),
    ];
    for (t, expected) in cases {
        let v = EpicsValue::from_bytes_array(*t, &[], 0).unwrap();
        assert_eq!(&v, expected, "DBF type {t:?}");
        // to_bytes round-trip: empty array encodes to empty payload,
        // which decodes back to the same empty array variant.
        let bytes = v.to_bytes();
        assert_eq!(bytes.len(), 0, "encoded length for empty {t:?}");
        let back = EpicsValue::from_bytes_array(*t, &bytes, 0).unwrap();
        assert_eq!(back, *expected, "round-trip {t:?}");
    }
}

/// R6-74 — the server's DBR_STRING reply of an over-long stored string must
/// TRUNCATE and succeed, not error.
///
/// C `getStringString` (`dbConvert.c:132-154`) is the read-side conversion for
/// a `DBF_STRING` field: it caps the copy at `MAX_STRING_SIZE - 1 = 39` bytes,
/// force-NUL-terminates, and returns 0. A `DESC` field is `size(41)`, so a C
/// IOC really can hold 40 chars and really does serve them as 39 + NUL — no
/// `ECA_BADCOUNT`, which is raised only on the libca *put* side
/// (`nciu::stringVerify`, already enforced by `validate_put_strings` on every
/// client put entry point).
///
/// This test pins the boundary so the truncation is not "fixed" into an error:
/// on the reply path, truncating IS the C behaviour.
#[test]
fn r6_74_server_string_reply_truncates_to_39_bytes_and_nul_like_c_getstringstring() {
    // 39 bytes — the longest string that survives intact.
    let exact = "a".repeat(39);
    let bytes = EpicsValue::String(exact.clone().into()).to_bytes();
    assert_eq!(bytes.len(), 40, "DBR_STRING is a fixed 40-byte field");
    assert_eq!(&bytes[..39], exact.as_bytes());
    assert_eq!(bytes[39], 0, "byte 39 must be the NUL terminator");

    // 40 and 45 bytes — C copies 39 and NUL-terminates (strncpy + pdst[39] = 0).
    for len in [40usize, 45] {
        let long = "b".repeat(len);
        let bytes = EpicsValue::String(long.clone().into()).to_bytes();
        assert_eq!(bytes.len(), 40, "the field stays 40 bytes wide");
        assert_eq!(
            &bytes[..39],
            &long.as_bytes()[..39],
            "the first 39 bytes are copied verbatim"
        );
        assert_eq!(bytes[39], 0, "byte 39 must be the NUL terminator");
        // …and it decodes back to the 39-byte prefix, exactly what a C client
        // of a C IOC would see.
        match EpicsValue::from_bytes(DbFieldType::String, &bytes).unwrap() {
            EpicsValue::String(s) => assert_eq!(s.as_bytes(), &long.as_bytes()[..39]),
            other => panic!("expected String, got {other:?}"),
        }
    }
}

/// A2-edge: the DBR_TIME encoder refuses a clock it cannot represent instead
/// of putting a fabricated instant on the wire.
///
/// One case per boundary of `epicsUInt32 secPastEpoch`, not one per story:
/// the unset sentinel, the epoch itself, one second below it, the last
/// representable second, and one past it. C wraps at both ends and returns
/// `epicsTimeOK` (`epicsTime.cpp:305-310` at `R7.0.10`); this port refuses,
/// which is the deliberate deviation the row asked for — a gap an archiver
/// records as a gap, rather than a wrong time it records as fact.
mod timestamp_range {
    use super::*;
    use epics_base_rs::types::wall_clock_range_warning;

    fn secs_field(at: SystemTime) -> u32 {
        let data = serialize_dbr(20, &EpicsValue::Double(1.0), 0, 0, at).unwrap();
        u32::from_be_bytes(data[4..8].try_into().unwrap())
    }

    #[test]
    fn the_unset_stamp_still_encodes_as_the_uninitialized_zero() {
        // C's uninitialized `epicsTimeStamp` is `{0, 0}` and every consumer —
        // including C's own `epicsTimeToStrftime` test at `epicsTime.cpp:176`
        // — looks for exactly that. A record that has not processed must not
        // become a stamp in 2086.
        assert_eq!(secs_field(SystemTime::UNIX_EPOCH), 0);
    }

    #[test]
    fn the_epics_epoch_itself_encodes_as_zero() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
        assert_eq!(secs_field(at), 0);
    }

    #[test]
    fn one_second_before_the_epics_epoch_wraps_exactly_as_c_wraps_it() {
        // C: `epicsInt64(src) - POSIX_TIME_AT_EPICS_EPOCH` = -1, assigned into
        // an `epicsUInt32` = 0xFFFF_FFFF (`epicsTime.cpp:305-310`). The clamp
        // this replaced answered 0 here, which C never answers.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS - 1);
        assert_eq!(secs_field(at), u32::MAX);
    }

    #[test]
    fn the_last_representable_second_still_encodes() {
        let at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + u32::MAX as u64);
        assert_eq!(secs_field(at), u32::MAX);
    }

    #[test]
    fn one_second_past_the_last_representable_second_wraps_to_zero() {
        // 2^32 mod 2^32 == 0 — again what C's assignment does, and again not
        // what the clamp did (it answered 0xFFFF_FFFF).
        let at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + u32::MAX as u64 + 1);
        assert_eq!(secs_field(at), 0);
    }

    // The wrap is C's, so it is not reportable per read. These four fix where
    // it IS reported: once, from init, at both ends of the range.

    #[test]
    fn a_clock_inside_the_range_warns_about_nothing() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS);
        assert!(wall_clock_range_warning(at).is_none());
        let last = SystemTime::UNIX_EPOCH
            + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + u32::MAX as u64);
        assert!(wall_clock_range_warning(last).is_none());
    }

    #[test]
    fn the_unset_stamp_is_not_a_clock_reading_and_warns_about_nothing() {
        assert!(wall_clock_range_warning(SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn a_pre_1990_clock_warns() {
        // The RTEMS row's actual case: `EPICS_RTEMS_BOOT_EPOCH` below 1990.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS - 1);
        let msg = wall_clock_range_warning(at).expect("a pre-1990 clock must be named");
        assert!(msg.contains("wall clock"), "{msg}");
        assert!(msg.contains("wrapped"), "{msg}");
    }

    #[test]
    fn a_post_2106_clock_warns() {
        let at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(EPICS_UNIX_EPOCH_OFFSET_SECS + u32::MAX as u64 + 1);
        assert!(wall_clock_range_warning(at).is_some());
    }
}
