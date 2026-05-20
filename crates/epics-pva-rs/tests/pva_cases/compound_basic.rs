//! pvxs non-array compound encodings — sub-`Structure`, regular
//! `Union`, regular `Variant` (pvxs `AnyTy`).
//!
//! pvxs reference: `src/dataencode.cpp` `to_wire_field` non-array
//! arms. The Rust `Structure` arm writes child field bytes back-to-
//! back (no inner bitset — only `encode_pv_field_with_bitset`
//! carries marks); `Union` writes `Size(selector)` + value or the
//! `0xFF` null sentinel; `Variant` writes type descriptor + value,
//! or `0xFF` for "no descriptor".
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (extracted from `to_wire_valid` on a holder structure, with the
//! leading BitSet header stripped).

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, VariantValue,
};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_sub_structure_two_fields_be() {
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    };
    let val = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            ("a".into(), PvField::Scalar(ScalarValue::Int(0x0102_0304))),
            ("b".into(), PvField::Scalar(ScalarValue::Double(1.0))),
        ],
    });
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("sub_structure_two_fields_be"));
}

#[test]
fn golden_pvxs_union_present_int_selector_be() {
    let desc = FieldDesc::Union {
        struct_id: String::new(),
        variants: vec![
            ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("f".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    };
    let val = PvField::Union {
        selector: 0,
        variant_name: "i".into(),
        value: Box::new(PvField::Scalar(ScalarValue::Int(7))),
    };
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("union_present_int_selector_be"));
}

#[test]
fn golden_pvxs_union_null_selector_emits_size_null() {
    let desc = FieldDesc::Union {
        struct_id: String::new(),
        variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let val = PvField::Union {
        selector: -1,
        variant_name: String::new(),
        value: Box::new(PvField::Scalar(ScalarValue::Int(0))),
    };
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("union_null_selector"));
}

#[test]
fn golden_pvxs_variant_present_int_be() {
    let val = PvField::Variant(Box::new(VariantValue {
        desc: Some(FieldDesc::Scalar(ScalarType::Int)),
        value: PvField::Scalar(ScalarValue::Int(9)),
    }));
    let mut out = Vec::new();
    encode_pv_field(&val, &FieldDesc::Variant, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("variant_present_int_be"));
}

#[test]
fn golden_pvxs_variant_null_descriptor_emits_ff() {
    let val = PvField::Variant(Box::new(VariantValue {
        desc: None,
        value: PvField::Scalar(ScalarValue::Int(0)),
    }));
    let mut out = Vec::new();
    encode_pv_field(&val, &FieldDesc::Variant, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("variant_null_descriptor"));
}
