//! pvxs non-array compound encodings — sub-`Structure`, regular
//! `Union`, regular `Variant` (pvxs `AnyTy`).
//!
//! pvxs reference: `src/dataencode.cpp` `to_wire_field` non-array
//! arms (StructTy, UnionTy, AnyTy). The Rust encoder's `Structure`
//! arm writes child field bytes back-to-back (no inner bitset
//! header — only `encode_pv_field_with_bitset` carries marks);
//! `Union` writes `Size(selector)` + value or the `0xFF` null
//! sentinel; `Variant` writes the type descriptor followed by the
//! value, or `0xFF` for "no descriptor".

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, VariantValue,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_sub_structure_two_fields_be() {
    // structure { int32 a; float64 b }, a=0x01020304, b=1.0 BE.
    // Children encoded back-to-back: Int(4) + Double(8) = 12 bytes,
    // no inner bitset header.
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
    assert_eq!(hex(&out), "010203043ff0000000000000");
}

#[test]
fn golden_pvxs_union_present_int_selector_be() {
    // union { int i; float64 f } with selector=0 (i=7) BE.
    // Wire shape: Size(0) + Int(7) = "00 00000007".
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
    assert_eq!(hex(&out), "0000000007");
}

#[test]
fn golden_pvxs_union_null_selector_emits_size_null() {
    // selector = -1 (no variant chosen) → Size-null sentinel 0xFF;
    // no value bytes follow. Out-of-range selectors fall through
    // the same path in `encode_pv_field` (see Union arm comment).
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
    assert_eq!(hex(&out), "ff");
}

#[test]
fn golden_pvxs_variant_present_int_be() {
    // Variant with descriptor=Int: emits the 1-byte type code 0x22
    // (pvxs typeCodeLUT Int) followed by the int bytes.
    let val = PvField::Variant(Box::new(VariantValue {
        desc: Some(FieldDesc::Scalar(ScalarType::Int)),
        value: PvField::Scalar(ScalarValue::Int(9)),
    }));
    let mut out = Vec::new();
    encode_pv_field(&val, &FieldDesc::Variant, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), "2200000009");
}

#[test]
fn golden_pvxs_variant_null_descriptor_emits_ff() {
    // No descriptor → 0xFF (pvxs Null type tag); no value follows.
    let val = PvField::Variant(Box::new(VariantValue {
        desc: None,
        value: PvField::Scalar(ScalarValue::Int(0)),
    }));
    let mut out = Vec::new();
    encode_pv_field(&val, &FieldDesc::Variant, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), "ff");
}
