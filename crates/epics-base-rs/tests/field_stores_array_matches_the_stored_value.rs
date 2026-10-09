//! `FieldDeclaration::field_stores_array` answers the put row's shape question
//! without cloning the field; this ratchets it against the clone it replaced,
//! `get_field(..).is_array()`, for every field of every vendored type.

mod module_records;

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::dbd_generated::RECORD_TYPES;
use epics_base_rs::server::record::{
    FieldDeclaration, FieldDesc, ProcessOutcome, Record, coerce_put_value,
};
use epics_base_rs::types::c_parse::Converted;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

fn assert_shapes_agree(name: &str, r: &dyn Record) {
    for d in r.field_list() {
        if let Some(v) = r.get_field(d.name) {
            assert_eq!(
                r.field_stores_array(d.name),
                v.is_array(),
                "{name}.{}",
                d.name
            );
        }
    }
}

#[test]
fn every_type_stores_the_shape_get_field_serves() {
    for rt in RECORD_TYPES {
        let rec = module_records::create_any(rt).unwrap();
        assert_shapes_agree(rt, &*rec);
    }
}

/// An `aSub` channel's shape follows its FTx/NOx once `init_record` has run.
#[epics_macros_rs::epics_test]
async fn an_asub_channel_stores_the_shape_its_count_gives_it() {
    let db = r#"
record(aSub, "AS3") { field(FTA, "DOUBLE") field(NOA, "3") field(FTVA, "LONG") field(NOVA, "3") }
record(aSub, "ASS") { field(FTA, "STRING") field(FTVA, "STRING") }
"#;
    let (pdb, _) = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for name in ["AS3", "ASS"] {
        let rec = pdb.get_record(name).unwrap();
        assert_shapes_agree(name, &*rec.read().record);
    }
}

/// A record type with no generated `.dbd`, as a test or an application may
/// write one.
struct Undeclared;

impl Record for Undeclared {
    fn record_type(&self) -> &'static str {
        "undeclared_array_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then(|| EpicsValue::DoubleArray(vec![0.0; 3].into()))
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Without a generated `.dbd` the stored value answers, so a string still
/// takes the array row into that buffer.
#[test]
fn an_undeclared_field_stores_the_shape_of_its_value() {
    assert_shapes_agree("undeclared", &Undeclared);
    let put = coerce_put_value(
        &Undeclared,
        "VAL",
        DbFieldType::Double,
        EpicsValue::String(PvString::from("1")),
    )
    .unwrap();
    assert!(matches!(put, Converted::Stored(EpicsValue::DoubleArray(ref a)) if a[..] == [1.0]));
}
