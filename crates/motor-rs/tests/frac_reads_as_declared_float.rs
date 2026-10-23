//! motorRecord.dbd:226 declares FRAC `DBF_FLOAT`; C stores a `float`, so a
//! DOUBLE get of 0.3 answers 0.30000001192092896.

use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::record::MotorRecord;

#[test]
fn frac_reads_as_declared_float() {
    let mut rec = MotorRecord::default();
    // A runtime put: C special() clamps FRAC (motorRecord.cc:2816-2829).
    rec.internal.init_invariants_synced = true;
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(1.0)));
    rec.put_field("FRAC", EpicsValue::Double(0.3)).unwrap();
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(0.3)));
    rec.put_field("FRAC", EpicsValue::Float(9.0)).unwrap();
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(1.5)));
    // The Double arm clamps to [0.1, 1.5] both ways, as the Float arm does.
    rec.put_field("FRAC", EpicsValue::Double(9.0)).unwrap();
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(1.5)));
    rec.put_field("FRAC", EpicsValue::Double(0.01)).unwrap();
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(0.1)));
    rec.put_field("FRAC", EpicsValue::Float(0.01)).unwrap();
    assert_eq!(rec.get_field("FRAC"), Some(EpicsValue::Float(0.1)));
}

/// motorRecord.dbd:576-632 declares RVAL/LRVL/RDIF/RRBV/RMP/REP/RVEL
/// `DBF_LONG` (epicsInt32); none of them is served as Int64.
#[test]
fn raw_fields_read_as_declared_long() {
    use epics_base_rs::server::record::FieldDeclaration;
    use epics_base_rs::types::DbFieldType;
    let rec = MotorRecord::default();
    for f in ["RVAL", "LRVL", "RDIF", "RRBV", "RMP", "REP", "RVEL"] {
        assert!(
            matches!(rec.get_field(f), Some(EpicsValue::Long(0))),
            "{f} reads as Long"
        );
        let declared = rec
            .field_list()
            .iter()
            .find(|d| d.name == f)
            .unwrap_or_else(|| panic!("{f} declared"))
            .dbf_type;
        assert_eq!(declared, DbFieldType::Long, "{f} declared DBF_LONG");
    }
}
