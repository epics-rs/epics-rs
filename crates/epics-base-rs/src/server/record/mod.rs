mod alarm;
mod common_fields;
mod link;
mod record_instance;
mod record_trait;
mod scan;

// Re-export all public types so existing imports continue to work.
pub use alarm::{AlarmSeverity, AnalogAlarmConfig};
pub use common_fields::CommonFields;
pub use link::{DbLink, LinkAddress, LinkProcessPolicy, ParsedLink, parse_link, parse_link_v2};
pub use record_instance::RecordInstance;
pub use record_trait::{
    CommonFieldPutResult, FieldDesc, ProcessSnapshot, Record, RecordProcessResult, SubroutineFn,
};
pub use scan::ScanType;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::ao::AoRecord;
    use crate::server::records::bi::BiRecord;
    use crate::server::records::stringin::StringinRecord;

    use crate::error::{CaError, CaResult};
    use crate::types::{DbFieldType, EpicsValue};

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
        rec.put_field("EGU", EpicsValue::String("celsius".into())).unwrap();
        match rec.get_field("EGU") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "celsius"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_ai_field_list() {
        let rec = AiRecord::default();
        let fields = rec.field_list();
        assert_eq!(fields.len(), 24); // 20 base + 4 sim fields
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
        rec.put_field("ZNAM", EpicsValue::String("Off".into())).unwrap();
        rec.put_field("ONAM", EpicsValue::String("On".into())).unwrap();
        match rec.get_field("ZNAM") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Off"),
            other => panic!("expected String, got {:?}", other),
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
        #[record(type = "test")]
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

        // Can read the read-only field
        match rec.get_field("NAME") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "fixed"),
            other => panic!("expected String, got {:?}", other),
        }

        // Cannot write the read-only field
        let result = rec.put_field("NAME", EpicsValue::String("changed".into()));
        assert!(result.is_err());

        // Can still write the mutable field
        rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 2.0).abs() < 1e-10),
            other => panic!("expected Double(2.0), got {:?}", other),
        }

        // Verify field_list read_only flag
        let fields = rec.field_list();
        assert!(!fields[0].read_only); // VAL
        assert!(fields[1].read_only);  // NAME
    }

    #[test]
    fn test_parse_pv_name() {
        use crate::server::database::parse_pv_name;
        assert_eq!(parse_pv_name("TEMP"), ("TEMP", "VAL"));
        assert_eq!(parse_pv_name("TEMP.EGU"), ("TEMP", "EGU"));
        assert_eq!(parse_pv_name("TEMP.HOPR"), ("TEMP", "HOPR"));
        assert_eq!(parse_pv_name("A.B.C"), ("A.B", "C"));
    }

    #[test]
    fn test_resolve_field_priority() {
        let rec = AiRecord::new(25.0);
        let instance = RecordInstance::new("TEMP".into(), rec);

        // Record field
        assert!(matches!(instance.resolve_field("VAL"), Some(EpicsValue::Double(_))));
        // Common field
        assert!(matches!(instance.resolve_field("SEVR"), Some(EpicsValue::Short(0))));
        assert!(matches!(instance.resolve_field("SCAN"), Some(EpicsValue::Enum(0))));
        // Virtual field
        match instance.resolve_field("NAME") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "TEMP"),
            other => panic!("expected String(TEMP), got {:?}", other),
        }
        match instance.resolve_field("RTYP") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "ai"),
            other => panic!("expected String(ai), got {:?}", other),
        }
        // Analog alarm fields available for ai
        assert!(instance.resolve_field("HIHI").is_some());
        // Unknown field
        assert!(instance.resolve_field("NONEXISTENT").is_none());
    }

    #[test]
    fn test_common_field_put() {
        let rec = AiRecord::new(25.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        // Set SCAN
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        assert!(matches!(result, CommonFieldPutResult::ScanChanged { .. })); // SCAN changed
        assert_eq!(instance.common.scan, ScanType::Sec1);

        // Set analog alarm threshold
        instance.put_common_field("HIHI", EpicsValue::Double(100.0)).unwrap();
        assert_eq!(instance.common.analog_alarm.as_ref().unwrap().hihi, 100.0);
    }

    #[test]
    fn test_evaluate_alarms() {
        use crate::server::recgbl;
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);
        instance.common.udf = false; // Clear UDF so it doesn't interfere

        // Set alarm thresholds
        instance.put_common_field("HIHI", EpicsValue::Double(100.0)).unwrap();
        instance.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16)).unwrap();
        instance.put_common_field("HIGH", EpicsValue::Double(80.0)).unwrap();
        instance.put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16)).unwrap();

        // No alarm
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

        // HIGH alarm
        instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // HIHI alarm
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
        // Empty
        assert_eq!(parse_link_v2(""), ParsedLink::None);
        assert_eq!(parse_link_v2("  "), ParsedLink::None);

        // Integer constant
        assert_eq!(parse_link_v2("42"), ParsedLink::Constant("42".to_string()));

        // Float constant (was bug: "3.14" used to parse as DB link record="3", field="14")
        assert_eq!(parse_link_v2("3.14"), ParsedLink::Constant("3.14".to_string()));
        assert_eq!(parse_link_v2("-1.5"), ParsedLink::Constant("-1.5".to_string()));

        // DB link — bare record name
        assert_eq!(parse_link_v2("TEMP"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "VAL".into(),
            policy: LinkProcessPolicy::ProcessPassive,
        }));

        // DB link — record.field
        assert_eq!(parse_link_v2("TEMP.EGU"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "EGU".into(),
            policy: LinkProcessPolicy::ProcessPassive,
        }));

        // DB link with NPP
        assert_eq!(parse_link_v2("TEMP.EGU NPP"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "EGU".into(),
            policy: LinkProcessPolicy::NoProcess,
        }));

        // CA/PVA protocols
        assert_eq!(parse_link_v2("ca://PV:NAME"), ParsedLink::Ca("PV:NAME".to_string()));
        assert_eq!(parse_link_v2("pva://PV:NAME"), ParsedLink::Pva("PV:NAME".to_string()));

        // Quoted string constant
        assert_eq!(parse_link_v2("\"hello\""), ParsedLink::Constant("hello".to_string()));

        // Constant value extraction
        let c = parse_link_v2("3.14");
        assert_eq!(c.constant_value(), Some(EpicsValue::Double(3.14)));
        let c = parse_link_v2("\"hello\"");
        assert_eq!(c.constant_value(), Some(EpicsValue::String("hello".into())));
        assert_eq!(parse_link_v2("TEMP").constant_value(), None);
    }

    #[test]
    fn test_link_cache_invalidation() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        assert_eq!(instance.parsed_inp, ParsedLink::None);
        instance.put_common_field("INP", EpicsValue::String("SOURCE.VAL".into())).unwrap();
        if let ParsedLink::Db(ref db) = instance.parsed_inp {
            assert_eq!(db.record, "SOURCE");
        } else {
            panic!("expected Db link");
        }

        // Change link -> cache updated
        instance.put_common_field("INP", EpicsValue::String("OTHER".into())).unwrap();
        if let ParsedLink::Db(ref db) = instance.parsed_inp {
            assert_eq!(db.record, "OTHER");
            assert_eq!(db.field, "VAL");
        } else {
            panic!("expected Db link");
        }

        // Clear link -> cache cleared
        instance.put_common_field("INP", EpicsValue::String("".into())).unwrap();
        assert_eq!(instance.parsed_inp, ParsedLink::None);
    }

    #[test]
    fn test_ai_linear_conversion() {
        let mut rec = AiRecord::default();
        rec.linr = 1; // LINEAR
        rec.eguf = 100.0;
        rec.egul = 0.0;
        rec.eslo = 1.0;
        rec.roff = 0;
        rec.aslo = 1.0;
        rec.aoff = 0.0;

        rec.rval = 50;
        rec.process().unwrap();
        // (50 + 0) * 1.0 + 0.0 * 1.0 + 0.0 = 50.0
        assert!((rec.val - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_ai_linear_with_offsets() {
        let mut rec = AiRecord::default();
        rec.linr = 1;
        rec.egul = 10.0;
        rec.eslo = 0.5;
        rec.roff = 100;
        rec.aslo = 2.0;
        rec.aoff = 5.0;

        rec.rval = 200;
        rec.process().unwrap();
        // (200 + 100) * 2.0 + 5.0 = 605.0
        // 605.0 * 0.5 + 10.0 = 312.5
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
        // First process: no smoothing (init was false)
        assert!((rec.val - 100.0).abs() < 1e-10);
        assert!(rec.init);

        // Second process with same value — should be 100 * 0.5 + 100 * 0.5 = 100
        rec.rval = 200;
        rec.process().unwrap();
        // new_val = 200 * 0.5 + 100 * 0.5 = 150
        assert!((rec.val - 150.0).abs() < 1e-10);
    }

    #[test]
    fn test_ai_no_conversion() {
        let mut rec = AiRecord::default();
        rec.linr = 0; // NO_CONVERSION
        rec.val = 42.0; // Set directly (as soft channel would)
        rec.process().unwrap();
        // VAL should be unchanged
        assert!((rec.val - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_common_fields_desc() {
        let rec = AiRecord::new(25.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        // DESC is now a common field
        instance.put_common_field("DESC", EpicsValue::String("Temperature".into())).unwrap();
        match instance.get_common_field("DESC") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
            other => panic!("expected String, got {:?}", other),
        }
        // Also accessible via resolve_field (common level)
        match instance.resolve_field("DESC") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_common_fields_new() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // PHAS default
        assert_eq!(instance.common.phas, 0);
        instance.put_common_field("PHAS", EpicsValue::Short(2)).unwrap();
        assert_eq!(instance.common.phas, 2);

        // DISV default is 1
        assert_eq!(instance.common.disv, 1);

        // HYST
        instance.put_common_field("HYST", EpicsValue::Double(5.0)).unwrap();
        assert!((instance.common.hyst - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_hyst_alarm_hysteresis() {
        use crate::server::recgbl;
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);
        instance.common.udf = false;

        instance.put_common_field("HIGH", EpicsValue::Double(80.0)).unwrap();
        instance.put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16)).unwrap();
        instance.put_common_field("HYST", EpicsValue::Double(5.0)).unwrap();

        // Go into HIGH alarm at val=85 -> LALM=85
        instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=82: still >= HIGH=80, alarm stays -> LALM updated to 82
        instance.record.set_val(EpicsValue::Double(82.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=78: below HIGH=80, but |78-LALM(82)|=4 < hyst=5 -> stays in alarm
        instance.record.set_val(EpicsValue::Double(78.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=76: below HIGH=80, |76-82|=6 >= hyst=5 -> alarm clears
        instance.record.set_val(EpicsValue::Double(76.0)).unwrap();
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

        // First process: val=0, mlst=0 -> |0-0|=0, not > 5 -> no trigger
        // But first time should trigger (MDEL=0 means any change triggers)
        // With MDEL=5, |0-0|=0 which is NOT > 5, so no trigger
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // VAL not included since |0-0| is not > 5
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=3: |3-0|=3, not > 5 -> no trigger
        instance.record.set_val(EpicsValue::Double(3.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=6: |6-0|=6 > 5 -> trigger, MLST updated to 6
        instance.record.set_val(EpicsValue::Double(6.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=10: |10-6|=4, not > 5 -> no trigger
        instance.record.set_val(EpicsValue::Double(10.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=12: |12-6|=6 > 5 -> trigger
        instance.record.set_val(EpicsValue::Double(12.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_deadband_mdel_zero() {
        let mut rec = AiRecord::default();
        rec.mdel = 0.0;
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // MDEL=0 means any change triggers
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // |0-0|=0, not > 0 -> no trigger (same value)
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        instance.record.set_val(EpicsValue::Double(0.001)).unwrap();
        let snap = instance.process_local().unwrap();
        // |0.001-0|=0.001 > 0 -> trigger
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_deadband_mdel_negative() {
        let mut rec = AiRecord::default();
        rec.mdel = -1.0;
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // MDEL < 0 means always trigger
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_bi_state_alarm() {
        use crate::server::recgbl;
        let mut rec = BiRecord::new(0);
        rec.zsv = AlarmSeverity::Major as i16;
        rec.osv = AlarmSeverity::Minor as i16;

        let mut instance = RecordInstance::new("SW".into(), rec);
        instance.common.udf = false;

        // VAL=0 -> ZSV=Major
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Major);

        // VAL=1 -> OSV=Minor
        instance.record.set_val(EpicsValue::Enum(1)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);
    }

    #[test]
    fn test_mbbi_state_alarm() {
        use crate::server::recgbl;
        use crate::server::records::mbbi::MbbiRecord;

        let mut rec = MbbiRecord::new(0);
        rec.onsv = AlarmSeverity::Minor as i16;
        rec.twsv = AlarmSeverity::Major as i16;

        let mut instance = RecordInstance::new("SEL".into(), rec);
        instance.common.udf = false;

        // VAL=0 -> ZRSV=0 (NoAlarm)
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

        // VAL=1 -> ONSV=Minor
        instance.record.set_val(EpicsValue::Enum(1)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // VAL=2 -> TWSV=Major
        instance.record.set_val(EpicsValue::Enum(2)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Major);
    }

    #[test]
    fn test_mbbi_unsv() {
        use crate::server::records::mbbi::MbbiRecord;

        let mut rec = MbbiRecord::new(0);
        rec.unsv = AlarmSeverity::Invalid as i16;

        let mut instance = RecordInstance::new("SEL".into(), rec);

        // VAL=15 -> FFSV=0 (NoAlarm), not UNSV
        instance.record.set_val(EpicsValue::Enum(15)).unwrap();
        instance.evaluate_alarms();
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_deadband_alarm_always_included() {
        let mut rec = AiRecord::default();
        rec.mdel = 100.0; // Very high deadband -- VAL never triggers
        let mut instance = RecordInstance::new("TEST".into(), rec);

        instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // VAL not included due to deadband
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
        // But SEVR/STAT/UDF always included
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "SEVR"));
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "STAT"));
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
        instance.common.lcnt = 5; // Pre-set to non-zero
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 0);
    }

    #[test]
    fn test_lcnt_increments_on_reentrance() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        // Simulate active processing
        instance.processing.store(true, std::sync::atomic::Ordering::Release);
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 1);
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 2);
    }

    #[test]
    fn test_lcnt_alarm_threshold() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        instance.processing.store(true, std::sync::atomic::Ordering::Release);
        for _ in 0..10 {
            let _ = instance.process_local().unwrap();
        }
        assert!(instance.common.lcnt >= 10);
        assert_eq!(instance.common.sevr, AlarmSeverity::Invalid);
        assert_eq!(instance.common.stat, 12); // SCAN_ALARM
    }

    #[test]
    fn test_lcnt_reset_on_success() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        instance.common.lcnt = 5;
        // processing is false (default), so process_local should succeed
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
        // Default is false
        match instance.get_common_field("DISP") {
            Some(EpicsValue::Char(0)) => {}
            other => panic!("expected Char(0), got {:?}", other),
        }
        // Set to true
        instance.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        assert!(instance.common.disp);
        match instance.get_common_field("DISP") {
            Some(EpicsValue::Char(1)) => {}
            other => panic!("expected Char(1), got {:?}", other),
        }
    }

    // --- PR 1: Hook Framework tests ---

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc as TestArc;

    /// Mock record that tracks special/validate_put/on_put calls via shared counters
    struct HookTrackingRecord {
        val: f64,
        special_before_count: TestArc<AtomicU32>,
        special_after_count: TestArc<AtomicU32>,
        on_put_count: TestArc<AtomicU32>,
        reject_field: Option<String>,
    }

    impl Record for HookTrackingRecord {
        fn record_type(&self) -> &'static str { "test_hook" }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                    else { Err(CaError::InvalidValue("bad type".into())) }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn field_list(&self) -> &'static [FieldDesc] {
            use crate::types::DbFieldType;
            static FIELDS: &[FieldDesc] = &[
                FieldDesc { name: "VAL", dbf_type: DbFieldType::Double, read_only: false },
            ];
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
        let special_before = TestArc::new(AtomicU32::new(0));
        let special_after = TestArc::new(AtomicU32::new(0));
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: special_before.clone(),
            special_after_count: special_after.clone(),
            on_put_count: TestArc::new(AtomicU32::new(0)),
            reject_field: None,
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("DESC", EpicsValue::String("hello".into())).unwrap();
        assert_eq!(special_before.load(Ordering::SeqCst), 1);
        assert_eq!(special_after.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_validate_put_rejects_common_field() {
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: TestArc::new(AtomicU32::new(0)),
            special_after_count: TestArc::new(AtomicU32::new(0)),
            on_put_count: TestArc::new(AtomicU32::new(0)),
            reject_field: Some("SCAN".into()),
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into()));
        assert!(result.is_err());
    }

    #[test]
    fn test_on_put_called_for_common_field() {
        let on_put = TestArc::new(AtomicU32::new(0));
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: TestArc::new(AtomicU32::new(0)),
            special_after_count: TestArc::new(AtomicU32::new(0)),
            on_put_count: on_put.clone(),
            reject_field: None,
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("DESC", EpicsValue::String("test".into())).unwrap();
        assert_eq!(on_put.load(Ordering::SeqCst), 1);
    }

    // --- PR 2: Scan Index tests ---

    #[test]
    fn test_phas_change_returns_result() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        // Set SCAN to non-Passive
        instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        // Now change PHAS
        let result = instance.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
        assert!(matches!(result, CommonFieldPutResult::PhasChanged { old_phas: 0, new_phas: 5, .. }));
    }

    #[test]
    fn test_phas_change_passive_no_result() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        // SCAN is Passive by default
        let result = instance.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
        assert_eq!(result, CommonFieldPutResult::NoChange);
    }

    #[test]
    fn test_scan_change_includes_phas() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("PHAS", EpicsValue::Short(3)).unwrap();
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        match result {
            CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
            other => panic!("expected ScanChanged, got {:?}", other),
        }
    }

    // --- PR 5: UDF Policy tests ---

    struct NoUdfClearRecord { val: f64 }
    impl Record for NoUdfClearRecord {
        fn record_type(&self) -> &'static str { "test_noudf" }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name { "VAL" => Some(EpicsValue::Double(self.val)), _ => None }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                    else { Err(CaError::InvalidValue("bad".into())) }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn field_list(&self) -> &'static [FieldDesc] { &[] }
        fn clears_udf(&self) -> bool { false }
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
        assert!(instance.common.udf); // UDF stays true
    }

    #[test]
    fn test_udf_alarm_persists() {
        use crate::server::recgbl;
        let rec = NoUdfClearRecord { val: 1.0 };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.common.udf = true;
        instance.process_local().unwrap();
        // UDF should still be true -> alarm check should produce UDF_ALARM
        assert!(instance.common.udf);
        // The process_local already ran evaluate_alarms + reset_alarms
        // With clears_udf=false, UDF stays true but evaluate_alarms was called before UDF check
        // Let's verify via another process cycle:
        instance.evaluate_alarms();
        let result = recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert!(result.alarm_changed || instance.common.sevr == AlarmSeverity::Invalid);
    }

    // ---- PR3: Snapshot generation tests ----

    #[test]
    fn test_snapshot_ai_with_display_metadata() {
        use crate::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(42.0);
        rec.egu = "degC".to_string();
        rec.prec = 3;
        rec.hopr = 100.0;
        rec.lopr = -50.0;
        let mut inst = RecordInstance::new("AI:TEST".into(), rec);
        inst.common.analog_alarm = Some(AnalogAlarmConfig {
            hihi: 90.0, high: 80.0, low: -20.0, lolo: -40.0,
            hhsv: AlarmSeverity::Major, hsv: AlarmSeverity::Minor,
            lsv: AlarmSeverity::Minor, llsv: AlarmSeverity::Major,
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
        // ai uses HOPR/LOPR as control limits
        let ctrl = snap.control.as_ref().unwrap();
        assert_eq!(ctrl.upper_ctrl_limit, 100.0);
        assert_eq!(ctrl.lower_ctrl_limit, -50.0);
        assert!(snap.enums.is_none());
    }

    #[test]
    fn test_snapshot_ao_with_drvh_drvl() {
        use crate::server::records::ao::AoRecord;
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
        use crate::server::records::bi::BiRecord;
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
        use crate::server::records::mbbi::MbbiRecord;
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
        // Empty strings for unset
        assert_eq!(enums.strings[3], "");
    }

    #[test]
    fn test_snapshot_longin_display() {
        use crate::server::records::longin::LonginRecord;
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
        use crate::server::records::stringin::StringinRecord;
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
        use crate::server::records::ai::AiRecord;
        let rec = AiRecord::new(1.0);
        let inst = RecordInstance::new("AI:TEST".into(), rec);
        assert!(inst.snapshot_for_field("NONEXISTENT").is_none());
    }

    #[test]
    fn test_snapshot_alarm_state() {
        use crate::server::records::ai::AiRecord;
        let rec = AiRecord::new(1.0);
        let mut inst = RecordInstance::new("AI:TEST".into(), rec);
        inst.common.stat = 7; // HIGH_ALARM
        inst.common.sevr = AlarmSeverity::Minor;

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.alarm.status, 7);
        assert_eq!(snap.alarm.severity, 1); // Minor = 1
    }
}
