//! A DBF_FLOAT or unsigned field reads in its declared variant, as C's dbAddr
//! carries the declared type.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::types::DbFieldType;
fn probe(name: &str, r: &dyn Record) -> Vec<String> {
    let mut bad = Vec::new();
    for d in r.field_list() {
        if d.runtime_typed || d.unreadable() {
            continue;
        }
        let Some(v) = r.get_field(d.name) else {
            continue;
        };
        if !matches!(
            d.dbf_type,
            DbFieldType::Float | DbFieldType::UChar | DbFieldType::UShort | DbFieldType::ULong
        ) {
            continue;
        }
        if v.db_field_type() != d.dbf_type {
            bad.push(format!(
                "{name}.{} decl {:?} got {:?}",
                d.name,
                d.dbf_type,
                v.db_field_type()
            ));
        }
    }
    bad
}
#[test]
fn float_and_unsigned_fields_read_in_their_declared_variant() {
    let mut bad = Vec::new();
    bad.extend(probe(
        "optics_rs::records::table::TableRecord",
        &optics_rs::records::table::TableRecord::default(),
    ));
    assert!(bad.is_empty(), "{bad:?}");
}
