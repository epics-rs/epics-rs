//! A DBF_FLOAT or unsigned field reads in its declared variant, as C's dbAddr
//! carries the declared type.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::types::DbFieldType;
use motor_rs::record::MotorRecord;

#[test]
fn float_and_unsigned_fields_read_in_their_declared_variant() {
    let rec = MotorRecord::default();
    let mut bad = Vec::new();
    for d in rec.field_list() {
        if d.runtime_typed || d.unreadable() {
            continue;
        }
        let Some(v) = rec.get_field(d.name) else {
            continue;
        };
        if matches!(
            d.dbf_type,
            DbFieldType::Float | DbFieldType::UChar | DbFieldType::UShort | DbFieldType::ULong
        ) && v.db_field_type() != d.dbf_type
        {
            bad.push(format!(
                "motor.{} decl {:?} got {:?}",
                d.name,
                d.dbf_type,
                v.db_field_type()
            ));
        }
    }
    assert!(bad.is_empty(), "{bad:?}");
}
