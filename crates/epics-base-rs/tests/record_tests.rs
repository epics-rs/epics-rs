#![allow(unused_imports, clippy::all)]
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[test]
fn test_ai_record_type() {
    let rec = AiRecord::new(25.0);
    assert_eq!(rec.record_type(), "ai");
}

#[test]
fn test_ai_get_val() {
    let rec = AiRecord::new(42.0);
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[test]
fn test_ai_put_val() {
    let mut rec = AiRecord::new(0.0);
    rec.put_field("VAL", EpicsValue::Double(99.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
        other => panic!("expected Double(99.0), got {:?}", other),
    }
}

#[test]
fn test_ai_string_field() {
    let mut rec = AiRecord::default();
    rec.put_field("EGU", EpicsValue::String("celsius".into()))
        .unwrap();
    match rec.get_field("EGU") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "celsius"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_ai_field_list() {
    let rec = AiRecord::default();
    let fields = rec.field_list();
    assert!(fields.len() >= 24); // 20 base + 4 sim fields
    assert_eq!(fields[0].name, "VAL");
    assert_eq!(fields[0].dbf_type, DbFieldType::Double);
    assert_eq!(fields[1].name, "EGU");
}

#[test]
fn test_ai_unknown_field() {
    let rec = AiRecord::default();
    assert!(rec.get_field("NONEXISTENT").is_none());
}

#[test]
fn test_ai_put_type_mismatch() {
    let mut rec = AiRecord::default();
    let result = rec.put_field("VAL", EpicsValue::String("bad".into()));
    assert!(result.is_err());
}

#[test]
fn test_ai_put_unknown_field() {
    let mut rec = AiRecord::default();
    let result = rec.put_field("NONEXISTENT", EpicsValue::Double(1.0));
    assert!(result.is_err());
}

#[test]
fn test_ao_record() {
    let mut rec = AoRecord::new(10.0);
    assert_eq!(rec.record_type(), "ao");
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 20.0).abs() < 1e-10),
        other => panic!("expected Double(20.0), got {:?}", other),
    }
}

#[test]
fn test_bi_record() {
    let mut rec = BiRecord::new(0);
    assert_eq!(rec.record_type(), "bi");
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 1),
        other => panic!("expected Enum(1), got {:?}", other),
    }
    rec.put_field("ZNAM", EpicsValue::String("Off".into()))
        .unwrap();
    rec.put_field("ONAM", EpicsValue::String("On".into()))
        .unwrap();
    match rec.get_field("ZNAM") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Off"),
        other => panic!("expected String, got {:?}", other),
    }
}

// epics-base f2fe9d12 (devBiSoftRaw): "Raw Soft Channel" INP reads
// must apply MASK to RVAL before the RVAL→VAL conversion. Verifies the
// `Record::apply_raw_input` override on BiRecord.
#[test]
fn test_bi_raw_soft_channel_applies_mask() {
    let mut rec = BiRecord::new(0);
    rec.mask = 0x0F;
    rec.apply_raw_input(EpicsValue::Long(0xFF)).unwrap();
    assert_eq!(rec.rval, 0x0F, "mask must clamp RVAL to low nibble");
    let _ = rec.process().unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 1, "masked-non-zero RVAL → VAL=1"),
        other => panic!("expected Enum, got {:?}", other),
    }
}

// MASK=0 must leave RVAL untouched (idempotent passthrough).
#[test]
fn test_bi_raw_soft_channel_mask_zero_passthrough() {
    let mut rec = BiRecord::new(0);
    rec.mask = 0;
    rec.apply_raw_input(EpicsValue::Long(0xDEAD_BEEFu32 as i32))
        .unwrap();
    assert_eq!(rec.rval, 0xDEAD_BEEFu32 as i32);
}

// A masked-to-zero raw read must yield VAL=0 even when the source
// had bits outside the mask set.
#[test]
fn test_bi_raw_soft_channel_mask_to_zero() {
    let mut rec = BiRecord::new(1);
    rec.mask = 0x01;
    rec.apply_raw_input(EpicsValue::Long(0xFE)).unwrap();
    assert_eq!(rec.rval, 0);
    let _ = rec.process().unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 0),
        other => panic!("expected Enum, got {:?}", other),
    }
}

#[test]
fn test_stringin_record() {
    let rec = StringinRecord::new("hello");
    assert_eq!(rec.record_type(), "stringin");
    match rec.get_field("VAL") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "hello"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_val_and_set_val() {
    let mut rec = AiRecord::new(5.0);
    match rec.val() {
        Some(EpicsValue::Double(v)) => assert!((v - 5.0).abs() < 1e-10),
        other => panic!("expected Double(5.0), got {:?}", other),
    }
    rec.set_val(EpicsValue::Double(10.0)).unwrap();
    match rec.val() {
        Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }
}

#[test]
fn test_record_instance() {
    let rec = AiRecord::new(25.0);
    let instance = RecordInstance::new("TEMP".into(), rec);
    assert_eq!(instance.name, "TEMP");
    match instance.record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 25.0).abs() < 1e-10),
        other => panic!("expected Double(25.0), got {:?}", other),
    }
}

#[test]
fn test_read_only_field() {
    use epics_macros_rs::EpicsRecord;

    #[derive(EpicsRecord)]
    #[record(type = "test", crate_path = "epics_base_rs")]
    struct TestRecord {
        #[field(type = "Double")]
        pub val: f64,
        #[field(type = "String", read_only)]
        pub name: String,
    }

    let mut rec = TestRecord {
        val: 1.0,
        name: "fixed".into(),
    };

    match rec.get_field("NAME") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "fixed"),
        other => panic!("expected String, got {:?}", other),
    }

    let result = rec.put_field("NAME", EpicsValue::String("changed".into()));
    assert!(result.is_err());

    rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 2.0).abs() < 1e-10),
        other => panic!("expected Double(2.0), got {:?}", other),
    }

    let fields = rec.field_list();
    assert!(!fields[0].read_only); // VAL
    assert!(fields[1].read_only); // NAME
}

#[test]
fn test_parse_pv_name() {
    use epics_base_rs::server::database::parse_pv_name;
    assert_eq!(parse_pv_name("TEMP"), ("TEMP", "VAL"));
    assert_eq!(parse_pv_name("TEMP.EGU"), ("TEMP", "EGU"));
    assert_eq!(parse_pv_name("TEMP.HOPR"), ("TEMP", "HOPR"));
    assert_eq!(parse_pv_name("A.B.C"), ("A.B", "C"));
}

#[test]
fn test_resolve_field_priority() {
    let rec = AiRecord::new(25.0);
    let instance = RecordInstance::new("TEMP".into(), rec);

    assert!(matches!(
        instance.resolve_field("VAL"),
        Some(EpicsValue::Double(_))
    ));
    assert!(matches!(
        instance.resolve_field("SEVR"),
        Some(EpicsValue::Short(0))
    ));
    assert!(matches!(
        instance.resolve_field("SCAN"),
        Some(EpicsValue::Enum(0))
    ));
    match instance.resolve_field("NAME") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "TEMP"),
        other => panic!("expected String(TEMP), got {:?}", other),
    }
    match instance.resolve_field("RTYP") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "ai"),
        other => panic!("expected String(ai), got {:?}", other),
    }
    assert!(instance.resolve_field("HIHI").is_some());
    assert!(instance.resolve_field("NONEXISTENT").is_none());
}

#[test]
fn test_common_field_put() {
    let rec = AiRecord::new(25.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    let result = instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    assert!(matches!(result, CommonFieldPutResult::ScanChanged { .. }));
    assert_eq!(instance.common.scan, ScanType::Sec1);

    instance
        .put_common_field("HIHI", EpicsValue::Double(100.0))
        .unwrap();
    assert_eq!(instance.common.analog_alarm.as_ref().unwrap().hihi, 100.0);
}

#[test]
fn test_evaluate_alarms() {
    use epics_base_rs::server::recgbl;
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);
    instance.common.udf = false;

    instance
        .put_common_field("HIHI", EpicsValue::Double(100.0))
        .unwrap();
    instance
        .put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
        .unwrap();
    instance
        .put_common_field("HIGH", EpicsValue::Double(80.0))
        .unwrap();
    instance
        .put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16))
        .unwrap();

    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

    instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(105.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);
}

#[test]
fn test_parse_link() {
    assert!(parse_link("").is_none());

    let link = parse_link("TEMP").unwrap();
    assert_eq!(link.record, "TEMP");
    assert_eq!(link.field, "VAL");

    let link = parse_link("TEMP.EGU").unwrap();
    assert_eq!(link.record, "TEMP");
    assert_eq!(link.field, "EGU");

    let link = parse_link("TEMP.VAL PP").unwrap();
    assert_eq!(link.record, "TEMP");
    assert_eq!(link.field, "VAL");
    assert_eq!(link.policy, LinkProcessPolicy::ProcessPassive);

    let link = parse_link("TEMP.VAL NPP").unwrap();
    assert_eq!(link.policy, LinkProcessPolicy::NoProcess);
}

#[test]
fn test_parse_link_v2() {
    assert_eq!(parse_link_v2(""), ParsedLink::None);
    assert_eq!(parse_link_v2("  "), ParsedLink::None);

    assert_eq!(parse_link_v2("42"), ParsedLink::Constant("42".to_string()));
    assert_eq!(
        parse_link_v2("3.14"),
        ParsedLink::Constant("3.14".to_string())
    );
    assert_eq!(
        parse_link_v2("-1.5"),
        ParsedLink::Constant("-1.5".to_string())
    );

    assert_eq!(
        parse_link_v2("TEMP"),
        ParsedLink::Db(DbLink {
            record: "TEMP".into(),
            field: "VAL".into(),
            policy: LinkProcessPolicy::ProcessPassive,
            monitor_switch: MonitorSwitch::NoMaximize,
        })
    );

    assert_eq!(
        parse_link_v2("TEMP.EGU"),
        ParsedLink::Db(DbLink {
            record: "TEMP".into(),
            field: "EGU".into(),
            policy: LinkProcessPolicy::ProcessPassive,
            monitor_switch: MonitorSwitch::NoMaximize,
        })
    );

    assert_eq!(
        parse_link_v2("TEMP.EGU NPP"),
        ParsedLink::Db(DbLink {
            record: "TEMP".into(),
            field: "EGU".into(),
            policy: LinkProcessPolicy::NoProcess,
            monitor_switch: MonitorSwitch::NoMaximize,
        })
    );

    assert_eq!(
        parse_link_v2("ca://PV:NAME"),
        ParsedLink::Ca("PV:NAME".to_string())
    );
    assert_eq!(
        parse_link_v2("pva://PV:NAME"),
        ParsedLink::Pva("PV:NAME".to_string())
    );

    assert_eq!(
        parse_link_v2("\"hello\""),
        ParsedLink::Constant("hello".to_string())
    );

    let c = parse_link_v2("3.15");
    assert_eq!(c.constant_value(), Some(EpicsValue::Double(3.15)));
    let c = parse_link_v2("\"hello\"");
    assert_eq!(c.constant_value(), Some(EpicsValue::String("hello".into())));
    assert_eq!(parse_link_v2("TEMP").constant_value(), None);
}

#[test]
fn test_link_cache_invalidation() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    assert_eq!(instance.parsed_inp, ParsedLink::None);
    instance
        .put_common_field("INP", EpicsValue::String("SOURCE.VAL".into()))
        .unwrap();
    if let ParsedLink::Db(ref db) = instance.parsed_inp {
        assert_eq!(db.record, "SOURCE");
    } else {
        panic!("expected Db link");
    }

    instance
        .put_common_field("INP", EpicsValue::String("OTHER".into()))
        .unwrap();
    if let ParsedLink::Db(ref db) = instance.parsed_inp {
        assert_eq!(db.record, "OTHER");
        assert_eq!(db.field, "VAL");
    } else {
        panic!("expected Db link");
    }

    instance
        .put_common_field("INP", EpicsValue::String("".into()))
        .unwrap();
    assert_eq!(instance.parsed_inp, ParsedLink::None);
}

#[test]
fn test_ai_linear_conversion() {
    let mut rec = AiRecord::default();
    rec.linr = 1;
    rec.eguf = 100.0;
    rec.egul = 0.0;
    rec.eslo = 1.0;
    rec.roff = 0;
    rec.aslo = 1.0;
    rec.aoff = 0.0;

    rec.rval = 50;
    rec.process().unwrap();
    assert!((rec.val - 50.0).abs() < 1e-10);
}

#[test]
fn test_ai_linear_with_offsets() {
    let mut rec = AiRecord::default();
    rec.linr = 2;
    rec.eoff = 10.0;
    rec.eslo = 0.5;
    rec.roff = 100;
    rec.aslo = 2.0;
    rec.aoff = 5.0;

    rec.rval = 200;
    rec.process().unwrap();
    assert!((rec.val - 312.5).abs() < 1e-10);
}

#[test]
fn test_ai_smoothing() {
    let mut rec = AiRecord::default();
    rec.linr = 1;
    rec.eslo = 1.0;
    rec.aslo = 1.0;
    rec.smoo = 0.5;

    rec.rval = 100;
    rec.process().unwrap();
    assert!((rec.val - 100.0).abs() < 1e-10);
    assert!(rec.init);

    rec.rval = 200;
    rec.process().unwrap();
    assert!((rec.val - 150.0).abs() < 1e-10);
}

#[test]
fn test_ai_no_conversion() {
    let mut rec = AiRecord::default();
    rec.linr = 0;
    rec.rval = 42;
    rec.process().unwrap();
    assert!((rec.val - 42.0).abs() < 1e-10);
}

#[test]
fn test_common_fields_desc() {
    let rec = AiRecord::new(25.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    instance
        .put_common_field("DESC", EpicsValue::String("Temperature".into()))
        .unwrap();
    match instance.get_common_field("DESC") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
        other => panic!("expected String, got {:?}", other),
    }
    match instance.resolve_field("DESC") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_common_fields_new() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);

    assert_eq!(instance.common.phas, 0);
    instance
        .put_common_field("PHAS", EpicsValue::Short(2))
        .unwrap();
    assert_eq!(instance.common.phas, 2);

    assert_eq!(instance.common.disv, 1);

    instance
        .put_common_field("HYST", EpicsValue::Double(5.0))
        .unwrap();
    assert!((instance.common.hyst - 5.0).abs() < 1e-10);
}

#[test]
fn test_hyst_alarm_hysteresis() {
    use epics_base_rs::server::recgbl;
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);
    instance.common.udf = false;

    instance
        .put_common_field("HIGH", EpicsValue::Double(80.0))
        .unwrap();
    instance
        .put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16))
        .unwrap();
    instance
        .put_common_field("HYST", EpicsValue::Double(5.0))
        .unwrap();

    instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(82.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(78.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    // C: lalm=80, val=78 >= 80-5=75, so alarm stays Minor
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(76.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    // C: lalm=80, val=76 >= 80-5=75, alarm still Minor (within hysteresis)
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    // Below hysteresis: val=74 < 75, alarm clears
    instance.record.set_val(EpicsValue::Double(74.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
}

#[test]
fn test_deadband_mdel() {
    let mut rec = AiRecord::default();
    rec.mdel = 5.0;
    rec.adel = 0.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);

    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

    instance.record.set_val(EpicsValue::Double(3.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

    instance.record.set_val(EpicsValue::Double(6.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

    instance.record.set_val(EpicsValue::Double(10.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

    instance.record.set_val(EpicsValue::Double(12.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
}

#[test]
fn test_deadband_mdel_zero() {
    let mut rec = AiRecord::default();
    rec.mdel = 0.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);

    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

    instance.record.set_val(EpicsValue::Double(0.001)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
}

#[test]
fn test_deadband_mdel_negative() {
    let mut rec = AiRecord::default();
    rec.mdel = -1.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);

    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
}

#[test]
fn test_bi_state_alarm() {
    use epics_base_rs::server::recgbl;
    let mut rec = BiRecord::new(0);
    rec.zsv = AlarmSeverity::Major as i16;
    rec.osv = AlarmSeverity::Minor as i16;

    let mut instance = RecordInstance::new("SW".into(), rec);
    instance.common.udf = false;

    // bi STATE alarm lives in the `Record::check_alarms` hook (C
    // `biRecord.c::checkAlarms`); `process_local` calls it before
    // `evaluate_alarms`. Mirror that order here.
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);

    instance.record.set_val(EpicsValue::Enum(1)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);
}

#[test]
fn test_mbbi_state_alarm() {
    use epics_base_rs::server::recgbl;
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.onsv = AlarmSeverity::Minor as i16;
    rec.twsv = AlarmSeverity::Major as i16;

    let mut instance = RecordInstance::new("SEL".into(), rec);
    instance.common.udf = false;

    // mbbi STATE alarm lives in the `Record::check_alarms` hook (C
    // `mbbiRecord.c::checkAlarms`); `process_local` calls it before
    // `evaluate_alarms`. Mirror that order here.
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

    instance.record.set_val(EpicsValue::Enum(1)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Enum(2)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);
}

#[test]
fn test_mbbi_unsv() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.unsv = AlarmSeverity::Invalid as i16;

    let mut instance = RecordInstance::new("SEL".into(), rec);

    instance.record.set_val(EpicsValue::Enum(15)).unwrap();
    instance.evaluate_alarms();
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
}

#[test]
fn test_deadband_alarm_on_change_bypasses_value_deadband() {
    // C `recGbl.c:202-210` (`recGblResetAlarms`): SEVR is posted only
    // when `prev_sevr != new_sevr`, and STAT only when `stat_mask` is
    // set (sevr change / stat change / amsg change). The alarm-field
    // posts are NOT gated by the VAL monitor deadband (MDEL/ADEL) —
    // `db_post_events(&pdbc->stat, …)` runs independently of the
    // value-change check. This test verifies the C-correct behavior:
    // a genuine SEVR transition posts SEVR and STAT even though the
    // VAL change is smaller than MDEL and is therefore deadband-
    // filtered out of the same snapshot. `process_local` returns
    // SEVR/STAT in `alarm_posts` (each with its own C event mask),
    // not in the record-wide `changed_fields` snapshot.
    use epics_base_rs::server::recgbl::EventMask;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0; // VAL change of 1.0 is below the value deadband.
    let mut instance = RecordInstance::new("TEST".into(), rec);
    // HIGH=0.5/Major so VAL=1.0 trips a HIGH alarm — a real
    // NoAlarm -> Major SEVR transition.
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: 1000.0,
        high: 0.5,
        low: -1000.0,
        lolo: -2000.0,
        hhsv: AlarmSeverity::Major,
        hsv: AlarmSeverity::Major,
        lsv: AlarmSeverity::Minor,
        llsv: AlarmSeverity::Major,
    });

    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, alarm_posts) = instance.process_local().unwrap();
    // VAL is deadband-filtered (|1.0 - 0.0| < MDEL=100).
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    // SEVR / STAT are NOT in the record-wide snapshot — they ride
    // the per-field `alarm_posts` list instead.
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "SEVR"));
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "STAT"));
    // SEVR posted DBE_VALUE on a sevr change.
    let sevr_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "SEVR")
        .map(|(_, m)| *m);
    assert_eq!(
        sevr_mask,
        Some(EventMask::VALUE),
        "SEVR must post with DBE_VALUE only"
    );
    // STAT posted DBE_ALARM (sevr change) | DBE_VALUE (stat change).
    let stat_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "STAT")
        .map(|(_, m)| *m);
    assert_eq!(
        stat_mask,
        Some(EventMask::ALARM | EventMask::VALUE),
        "STAT must post with DBE_ALARM | DBE_VALUE on a sevr+stat change"
    );
    // Defect 2: AMSG must be posted alongside STAT with the SAME mask.
    // C `recGblResetAlarms` posts AMSG whenever any alarm field moved;
    // `process_local` previously omitted it entirely.
    let amsg_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "AMSG")
        .map(|(_, m)| *m);
    assert_eq!(
        amsg_mask, stat_mask,
        "AMSG must be posted with the same mask as STAT"
    );
}

#[test]
fn test_no_alarm_change_does_not_post_sevr_stat() {
    // C `recGbl.c:202-207`: when `prev_sevr == new_sevr` and
    // `prev_stat == new_stat`, `recGblResetAlarms` posts neither
    // SEVR nor STAT. A record processed with no alarm transition
    // must not emit alarm-field monitor events — neither in the
    // record-wide snapshot nor in the per-field `alarm_posts` list.
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "SEVR"));
    assert!(!snap.changed_fields.iter().any(|(k, _)| k == "STAT"));
    assert!(!alarm_posts.iter().any(|(f, _)| *f == "SEVR"));
    assert!(!alarm_posts.iter().any(|(f, _)| *f == "STAT"));
}

#[test]
fn test_pact_reads_zero_when_idle() {
    let instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    match instance.get_common_field("PACT") {
        Some(EpicsValue::Char(0)) => {}
        other => panic!("expected Char(0), got {:?}", other),
    }
}

#[test]
fn test_pact_write_rejected() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    let result = instance.put_common_field("PACT", EpicsValue::Char(1));
    assert!(matches!(result, Err(CaError::ReadOnlyField(_))));
}

#[test]
fn test_lcnt_zero_after_process() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.common.lcnt = 5;
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 0);
}

#[test]
fn test_lcnt_increments_on_reentrance() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance
        .processing
        .store(true, std::sync::atomic::Ordering::Release);
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 1);
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 2);
}

#[test]
fn test_lcnt_alarm_threshold() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance
        .processing
        .store(true, std::sync::atomic::Ordering::Release);
    for _ in 0..10 {
        let _ = instance.process_local().unwrap();
    }
    assert!(instance.common.lcnt >= 10);
    assert_eq!(instance.common.sevr, AlarmSeverity::Invalid);
    // C `menuAlarmStat.dbd`: SCAN_ALARM = 13.
    assert_eq!(
        instance.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SCAN_ALARM
    );
}

#[test]
fn test_lcnt_reset_on_success() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.common.lcnt = 5;
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 0);
}

#[test]
fn test_proc_reads_zero() {
    let instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    match instance.get_common_field("PROC") {
        Some(EpicsValue::Char(0)) => {}
        other => panic!("expected Char(0), got {:?}", other),
    }
}

#[test]
fn test_disp_get_put() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    match instance.get_common_field("DISP") {
        Some(EpicsValue::Char(0)) => {}
        other => panic!("expected Char(0), got {:?}", other),
    }
    instance
        .put_common_field("DISP", EpicsValue::Char(1))
        .unwrap();
    assert!(instance.common.disp);
    match instance.get_common_field("DISP") {
        Some(EpicsValue::Char(1)) => {}
        other => panic!("expected Char(1), got {:?}", other),
    }
}

// --- Hook Framework tests ---

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

struct HookTrackingRecord {
    val: f64,
    special_before_count: Arc<AtomicU32>,
    special_after_count: Arc<AtomicU32>,
    on_put_count: Arc<AtomicU32>,
    reject_field: Option<String>,
}

impl Record for HookTrackingRecord {
    fn record_type(&self) -> &'static str {
        "test_hook"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad type".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        static FIELDS: &[FieldDesc] = &[FieldDesc {
            name: "VAL",
            dbf_type: DbFieldType::Double,
            read_only: false,
        }];
        FIELDS
    }
    fn validate_put(&self, field: &str, _value: &EpicsValue) -> CaResult<()> {
        if let Some(ref reject) = self.reject_field {
            if field == reject {
                return Err(CaError::InvalidValue("rejected by validate_put".into()));
            }
        }
        Ok(())
    }
    fn on_put(&mut self, _field: &str) {
        self.on_put_count.fetch_add(1, Ordering::SeqCst);
    }
    fn special(&mut self, _field: &str, after: bool) -> CaResult<()> {
        if after {
            self.special_after_count.fetch_add(1, Ordering::SeqCst);
        } else {
            self.special_before_count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn test_special_called_on_common_put() {
    let special_before = Arc::new(AtomicU32::new(0));
    let special_after = Arc::new(AtomicU32::new(0));
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: special_before.clone(),
        special_after_count: special_after.clone(),
        on_put_count: Arc::new(AtomicU32::new(0)),
        reject_field: None,
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("DESC", EpicsValue::String("hello".into()))
        .unwrap();
    assert_eq!(special_before.load(Ordering::SeqCst), 1);
    assert_eq!(special_after.load(Ordering::SeqCst), 1);
}

#[test]
fn test_validate_put_rejects_common_field() {
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: Arc::new(AtomicU32::new(0)),
        special_after_count: Arc::new(AtomicU32::new(0)),
        on_put_count: Arc::new(AtomicU32::new(0)),
        reject_field: Some("SCAN".into()),
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into()));
    assert!(result.is_err());
}

#[test]
fn test_on_put_called_for_common_field() {
    let on_put = Arc::new(AtomicU32::new(0));
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: Arc::new(AtomicU32::new(0)),
        special_after_count: Arc::new(AtomicU32::new(0)),
        on_put_count: on_put.clone(),
        reject_field: None,
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("DESC", EpicsValue::String("test".into()))
        .unwrap();
    assert_eq!(on_put.load(Ordering::SeqCst), 1);
}

// --- Scan Index tests ---

#[test]
fn test_phas_change_returns_result() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    let result = instance
        .put_common_field("PHAS", EpicsValue::Short(5))
        .unwrap();
    assert!(matches!(
        result,
        CommonFieldPutResult::PhasChanged {
            old_phas: 0,
            new_phas: 5,
            ..
        }
    ));
}

#[test]
fn test_phas_change_passive_no_result() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let result = instance
        .put_common_field("PHAS", EpicsValue::Short(5))
        .unwrap();
    assert_eq!(result, CommonFieldPutResult::NoChange);
}

#[test]
fn test_scan_change_includes_phas() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("PHAS", EpicsValue::Short(3))
        .unwrap();
    let result = instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    match result {
        CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
        other => panic!("expected ScanChanged, got {:?}", other),
    }
}

// --- UDF Policy tests ---

struct NoUdfClearRecord {
    val: f64,
}
impl Record for NoUdfClearRecord {
    fn record_type(&self) -> &'static str {
        "test_noudf"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
    fn clears_udf(&self) -> bool {
        false
    }
}

#[test]
fn test_udf_cleared_after_process() {
    let rec = AiRecord::new(1.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    assert!(instance.common.udf);
    instance.process_local().unwrap();
    assert!(!instance.common.udf);
}

#[test]
fn test_udf_not_cleared_when_clears_udf_false() {
    let rec = NoUdfClearRecord { val: 1.0 };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    assert!(instance.common.udf);
    instance.process_local().unwrap();
    assert!(instance.common.udf);
}

#[test]
fn test_udf_alarm_persists() {
    use epics_base_rs::server::recgbl;
    let rec = NoUdfClearRecord { val: 1.0 };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.common.udf = true;
    instance.process_local().unwrap();
    assert!(instance.common.udf);
    instance.evaluate_alarms();
    let result = recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert!(result.alarm_changed || instance.common.sevr == AlarmSeverity::Invalid);
}

// ---- Snapshot generation tests ----

#[test]
fn test_snapshot_ai_with_display_metadata() {
    let mut rec = AiRecord::new(42.0);
    rec.egu = "degC".to_string();
    rec.prec = 3;
    rec.hopr = 100.0;
    rec.lopr = -50.0;
    let mut inst = RecordInstance::new("AI:TEST".into(), rec);
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: 90.0,
        high: 80.0,
        low: -20.0,
        lolo: -40.0,
        hhsv: AlarmSeverity::Major,
        hsv: AlarmSeverity::Minor,
        lsv: AlarmSeverity::Minor,
        llsv: AlarmSeverity::Major,
    });

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.value, EpicsValue::Double(42.0));
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.precision, 3);
    assert_eq!(disp.upper_disp_limit, 100.0);
    assert_eq!(disp.lower_disp_limit, -50.0);
    assert_eq!(disp.upper_alarm_limit, 90.0);
    assert_eq!(disp.upper_warning_limit, 80.0);
    assert_eq!(disp.lower_warning_limit, -20.0);
    assert_eq!(disp.lower_alarm_limit, -40.0);
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 100.0);
    assert_eq!(ctrl.lower_ctrl_limit, -50.0);
    assert!(snap.enums.is_none());
}

#[test]
fn test_snapshot_ao_with_drvh_drvl() {
    let mut rec = AoRecord::new(10.0);
    rec.egu = "V".to_string();
    rec.hopr = 100.0;
    rec.lopr = 0.0;
    rec.drvh = 50.0;
    rec.drvl = 5.0;
    let inst = RecordInstance::new("AO:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 50.0);
    assert_eq!(ctrl.lower_ctrl_limit, 5.0);
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "V");
}

#[test]
fn test_snapshot_bi_enum_strings() {
    let mut rec = BiRecord::new(0);
    rec.znam = "Off".to_string();
    rec.onam = "On".to_string();
    let inst = RecordInstance::new("BI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert!(snap.display.is_none());
    assert!(snap.control.is_none());
    let enums = snap.enums.as_ref().unwrap();
    assert_eq!(enums.strings.len(), 2);
    assert_eq!(enums.strings[0], "Off");
    assert_eq!(enums.strings[1], "On");
}

#[test]
fn test_snapshot_mbbi_16_strings() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let mut rec = MbbiRecord::default();
    rec.zrst = "Zero".to_string();
    rec.onst = "One".to_string();
    rec.twst = "Two".to_string();
    rec.ffst = "Fifteen".to_string();
    let inst = RecordInstance::new("MBBI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let enums = snap.enums.as_ref().unwrap();
    assert_eq!(enums.strings.len(), 16);
    assert_eq!(enums.strings[0], "Zero");
    assert_eq!(enums.strings[1], "One");
    assert_eq!(enums.strings[2], "Two");
    assert_eq!(enums.strings[15], "Fifteen");
    assert_eq!(enums.strings[3], "");
}

#[test]
fn test_snapshot_longin_display() {
    use epics_base_rs::server::records::longin::LonginRecord;
    let mut rec = LonginRecord::new(999);
    rec.egu = "counts".to_string();
    rec.hopr = 10000;
    rec.lopr = 0;
    let inst = RecordInstance::new("LONGIN:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "counts");
    assert_eq!(disp.precision, 0);
    assert_eq!(disp.upper_disp_limit, 10000.0);
    assert_eq!(disp.lower_disp_limit, 0.0);
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 10000.0);
    assert_eq!(ctrl.lower_ctrl_limit, 0.0);
}

#[test]
fn test_snapshot_stringin_no_metadata() {
    let rec = StringinRecord::new("hello");
    let inst = RecordInstance::new("SI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.value, EpicsValue::String("hello".to_string()));
    assert!(snap.display.is_none());
    assert!(snap.control.is_none());
    assert!(snap.enums.is_none());
}

#[test]
fn test_snapshot_field_not_found() {
    let rec = AiRecord::new(1.0);
    let inst = RecordInstance::new("AI:TEST".into(), rec);
    assert!(inst.snapshot_for_field("NONEXISTENT").is_none());
}

#[test]
fn test_snapshot_alarm_state() {
    let rec = AiRecord::new(1.0);
    let mut inst = RecordInstance::new("AI:TEST".into(), rec);
    inst.common.stat = 7;
    inst.common.sevr = AlarmSeverity::Minor;

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.alarm.status, 7);
    assert_eq!(snap.alarm.severity, 1);
}

// ---------------------------------------------------------------------------
// epics-base PR #817 integration regression tests.
//
// PR #817 (commit c9817fa59) bundled three changes:
//   (1) Add AFTC alarm-severity low-pass filter to bi record.
//   (2) Fix mbbi: write the new filter accumulator back to AFVL
//       (originally the local was discarded, so the filter never
//        retained state between cycles).
//   (3) Fix mbbi COSV/LALM: the `if (val == lalm || recGblSetSevr(...))
//       return;` short-circuit ate `recGblSetSevr`'s return as a
//       boolean and skipped the LALM update when COSV was non-zero,
//       so subsequent transitions were silently dropped.
//
// In epics-rs the filter is centralised in
// `RecordInstance::aftc_filter` and plumbed through `evaluate_alarms`.
// AFVL writeback and the LALM-always-update structure are already in
// place. These tests pin the post-PR-817 contract end-to-end.
// ---------------------------------------------------------------------------

/// (PR #817) bi record AFTC integration. C `biRecord.c::checkAlarms`
/// (biRecord.c:249-263) seeds the AFVL accumulator with the RAW
/// severity number — `afvl = (double) alarm;` (biRecord.c:252) — NOT
/// a unit sign. On the first (seed) sample `alarm` is left unchanged
/// so the raw severity passes through `recGblSetSevr` verbatim.
#[test]
fn test_bi_aftc_seeds_afvl_on_initial_sample() {
    let mut rec = BiRecord::new(0); // val=0 → ZSV path
    rec.zsv = AlarmSeverity::Major as i16;
    rec.aftc = 5.0;
    rec.afvl = 0.0; // signals "first sample"

    let mut inst = RecordInstance::new("BI:AFTC".into(), rec);
    inst.common.udf = false;

    // AFTC alarm filter runs inside `Record::check_alarms` (C
    // `biRecord.c::checkAlarms`), the hook `process_local` invokes.
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    epics_base_rs::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);

    // Initial sample: C `biRecord.c:251-252` `if (afvl == 0) afvl =
    // (double) alarm;` leaves `alarm` unchanged → raw severity passes
    // through.
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "initial AFTC sample must pass raw severity through"
    );
    // C `biRecord.c:252` — AFVL is seeded with the RAW severity number
    // (2.0 for MAJOR), NOT a unit sign.
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable");
    assert!(
        (afvl - 2.0).abs() < 1e-9,
        "AFVL seed must be the raw severity 2.0 for a MAJOR sample, got {afvl}"
    );
}

/// (PR #817) The AFTC filter delays the CLEARING of an alarm. C
/// `biRecord.c::checkAlarms` (biRecord.c:249-263): once AFVL holds a
/// non-zero severity, a NO_ALARM sample decays it by
/// `afvl = alpha*afvl + (1-alpha)*0` and the fold-back
/// `if (afvl - floor(afvl) > THRESHOLD) afvl = -afvl;` keeps
/// `abs(floor(afvl))` reporting the prior MAJOR until enough
/// NO_ALARM time has elapsed. A sustained MAJOR holds AFVL at 2.0.
#[test]
fn test_bi_aftc_delays_alarm_clear() {
    use std::time::Duration;

    // Direct unit test of the filter primitive across cycles.
    use epics_base_rs::server::records::bi::aftc_filter;
    let aftc = 10.0;
    let t0 = std::time::SystemTime::UNIX_EPOCH;

    // Cycle 1: seed with a MAJOR sample. C `biRecord.c:251-252`
    // `if (afvl == 0) afvl = (double) alarm;` → afvl = 2.0, alarm = 2.
    let (a1, afvl1) = aftc_filter(2, aftc, 0.0, t0, t0);
    assert_eq!(a1, 2, "MAJOR seed must report the raw MAJOR severity");
    assert!(
        (afvl1 - 2.0).abs() < 1e-9,
        "MAJOR seed must set afvl=2.0 (raw severity), got {afvl1}"
    );

    // Cycle 2: a single NO_ALARM sample 1s later. C
    // `biRecord.c:255-262`: alpha = 10/(1+10) ≈ 0.909,
    // afvl = 0.909*2 + 0.0 ≈ 1.818; fractional part 0.818 > 0.6321
    // → afvl = -1.818; alarm = abs(floor(-1.818)) = abs(-2) = 2.
    // The momentary clear is FILTERED — still reports MAJOR.
    let t1 = t0 + Duration::from_secs(1);
    let (a2, afvl2) = aftc_filter(0, aftc, afvl1, t0, t1);
    assert!(
        (afvl2 - (-1.0 * (10.0 / 11.0) * 2.0)).abs() < 1e-9,
        "one NO_ALARM sample must fold afvl to -1.818..., got {afvl2}"
    );
    assert_eq!(
        a2, 2,
        "a momentary alarm-clear must be FILTERED (still reports MAJOR)"
    );

    // Sustained NO_ALARM: feed NO_ALARM repeatedly until the reported
    // severity finally drops to 0.
    let mut afvl = afvl2;
    let mut t = t1;
    let mut reported = 2u16;
    for _ in 0..400 {
        let prev = t;
        t += Duration::from_secs(1);
        let (a, v) = aftc_filter(0, aftc, afvl, prev, t);
        afvl = v;
        reported = a;
        if reported == 0 {
            break;
        }
    }
    assert_eq!(
        reported, 0,
        "after sustained NO_ALARM the filter eventually clears the alarm"
    );

    // A sustained MAJOR holds the accumulator at 2.0 — C smoothing
    // `afvl = alpha*2 + (1-alpha)*2 = 2` is a fixed point.
    let (a3, afvl3) = aftc_filter(2, aftc, 2.0, t0, t1);
    assert_eq!(a3, 2, "sustained MAJOR keeps reporting MAJOR");
    assert!(
        (afvl3 - 2.0).abs() < 1e-9,
        "sustained MAJOR holds afvl at the fixed point 2.0, got {afvl3}"
    );
}

/// (PR #817 Fix #2) mbbi must write the new filter accumulator back
/// to AFVL on every evaluate_alarms call when AFTC>0. The pre-fix
/// C code computed the new value into a local but never assigned
/// `prec->afvl = afvl;` so the filter never retained state between
/// process cycles. The Rust port routes through
/// `record.put_field("AFVL", …)` after `aftc_filter`.
#[test]
fn test_mbbi_aftc_writes_afvl_back_each_cycle() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(1); // val=1 → ONSV
    rec.onsv = AlarmSeverity::Major as i16;
    rec.aftc = 2.0;
    rec.afvl = 0.0;

    let mut inst = RecordInstance::new("MBBI:AFTC".into(), rec);
    inst.common.udf = false;

    // AFTC alarm filter runs inside `Record::check_alarms` (C
    // `mbbiRecord.c::checkAlarms`), the hook `process_local` invokes.
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let afvl_after_first = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable after first cycle");
    assert!(
        afvl_after_first != 0.0,
        "AFVL must be non-zero after first AFTC cycle (was the writeback dropped?)"
    );
    // Second cycle with the same val keeps the filter state alive
    // and yields a positive accumulator (steady-state aim is 2.0).
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let afvl_after_second = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable after second cycle");
    assert!(
        afvl_after_second.abs() > 0.0,
        "AFVL must remain non-zero after the second cycle"
    );
}

/// (PR #817 Fix #3) mbbi LALM must be updated on every val change
/// even when COSV fires. The pre-fix C code wrote
/// `if (val == lalm || recGblSetSevr(prec, COS_ALARM, prec->cosv)) return;`
/// — `recGblSetSevr` returns the previous severity as a small int,
/// which when COSV≠0 was treated as truthy by the `||`, taking the
/// early return and skipping `prec->lalm = val`. The downstream
/// effect was that subsequent val transitions saw a stale LALM and
/// COSV failed to fire on the next change.
///
/// The Rust port already has the post-fix shape: the
/// `val != lalm` branch unconditionally writes LALM after firing
/// COS_ALARM. This test pins both halves of the bug:
///   (a) one transition with COSV≠NoAlarm bumps LALM to the new val;
///   (b) a subsequent transition still fires COS because LALM was
///       updated, and LALM advances again.
#[test]
fn test_mbbi_lalm_updates_when_cosv_set() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.cosv = AlarmSeverity::Major as i16; // pre-fix bug trigger
    rec.put_field("LALM", EpicsValue::Enum(0)).unwrap();

    let mut inst = RecordInstance::new("MBBI:LALM".into(), rec);
    inst.common.udf = false;

    // Transition 0 → 2: COS_ALARM fires (cosv=Major), LALM must
    // advance to 2. COS/LALM logic lives in `Record::check_alarms`
    // (C `mbbiRecord.c::checkAlarms`), the hook `process_local` runs.
    inst.record.set_val(EpicsValue::Enum(2)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm_after_first = inst
        .record
        .get_field("LALM")
        .and_then(|v| match v {
            EpicsValue::Enum(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm_after_first, 2,
        "LALM must advance to new val even when COSV fires"
    );

    // Transition 2 → 0: LALM must advance to 0. The pre-fix C bug
    // would have left LALM at 0 from the start, so this second
    // transition would have looked like "val == lalm" and the COS
    // path would have returned early without updating either.
    inst.record.set_val(EpicsValue::Enum(0)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm_after_second = inst
        .record
        .get_field("LALM")
        .and_then(|v| match v {
            EpicsValue::Enum(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm_after_second, 0,
        "LALM must advance again on the next transition"
    );
    // COS alarm must have re-fired during cycle 2: the accumulator
    // (`nsev`) records the highest severity hit since the last
    // reset_alarms call. With LALM correctly advanced from 2 to 0
    // between cycles, the val=2→0 step still triggers
    // `recGblSetSevr(COS_ALARM, Major)`.
    let nsev_after_second = inst.common.nsev;
    assert_eq!(
        nsev_after_second,
        AlarmSeverity::Major,
        "COS alarm must re-fire on the second transition (LALM-update bug regression)"
    );
}

/// Sibling regression for bi: same LALM-always-updates contract.
/// The Rust port handles bi and mbbi via the same `evaluate_alarms`
/// branch structure, so any regression in one implies a regression
/// in the other.
#[test]
fn test_bi_lalm_updates_when_cosv_set() {
    let mut rec = BiRecord::new(0);
    rec.cosv = AlarmSeverity::Major as i16;
    rec.put_field("LALM", EpicsValue::Enum(0)).unwrap();

    let mut inst = RecordInstance::new("BI:LALM".into(), rec);
    inst.common.udf = false;

    // COS/LALM logic lives in `Record::check_alarms` (C
    // `biRecord.c::checkAlarms`), the hook `process_local` runs.
    inst.record.set_val(EpicsValue::Enum(1)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm = inst
        .record
        .get_field("LALM")
        .and_then(|v| match v {
            EpicsValue::Enum(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm, 1,
        "bi LALM must advance to new val even when COSV fires"
    );
}
