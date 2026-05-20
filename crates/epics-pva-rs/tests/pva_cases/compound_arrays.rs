//! PVA-R13 wire-shape: compound-array per-element presence byte.
//!
//! pvxs reference: `src/dataencode.cpp`
//! - `:354-365` StructA encode (presence 0x00 | 0x01 + body)
//! - `:368-378` UnionA  encode (presence 0x00 | 0x01 + selector + value)
//! - `:382-393` AnyA    encode (presence 0x00 | 0x01 + descriptor + value)
//!
//! Pre-PVA-R13 Rust emitted the selector / descriptor inline with
//! `0xFF` as the null sentinel — a pvxs peer reads that as the
//! presence byte (≠ 0/1 → protocol fault). These goldens lock the
//! per-element shape so a future encoder refactor can't regress.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (extracted from `to_wire_valid` on a holder structure with the
//! leading BitSet stripped).

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::encode_pv_field;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, UnionItem, VariantValue,
};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_pvxs_struct_array_two_present() {
    // 2-element StructA where each element is `structure { int32 v }`.
    let desc = FieldDesc::StructureArray {
        struct_id: String::new(),
        fields: vec![("v".to_string(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let arr = PvField::StructureArray(vec![
        Some(PvStructure {
            struct_id: String::new(),
            fields: vec![(
                "v".to_string(),
                PvField::Scalar(ScalarValue::Int(0x0102_0304)),
            )],
        }),
        Some(PvStructure {
            struct_id: String::new(),
            fields: vec![(
                "v".to_string(),
                PvField::Scalar(ScalarValue::Int(0x0506_0708)),
            )],
        }),
    ]);
    let mut out = Vec::new();
    encode_pv_field(&arr, &desc, ByteOrder::Big, &mut out);
    // pvxs emits: Size(2) + presence(0x01) + int + presence(0x01) + int.
    assert_eq!(hex(&out), golden("struct_array_two_present"));
}

#[test]
fn golden_pvxs_union_array_present_int_selector() {
    // 1-element UnionArray with two variants `[int32, float64]`,
    // selecting int32 = 0x07.
    let desc = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![
            ("i".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ("f".to_string(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    };
    let arr = PvField::UnionArray(vec![Some(UnionItem {
        selector: 0,
        variant_name: "i".to_string(),
        value: PvField::Scalar(ScalarValue::Int(7)),
    })]);
    let mut out = Vec::new();
    encode_pv_field(&arr, &desc, ByteOrder::Big, &mut out);
    // pvxs emits: Size(1) + presence(0x01) + Size(0=selector) + Int(7) BE.
    assert_eq!(hex(&out), golden("union_array_present_int_selector"));
}

#[test]
fn golden_pvxs_variant_array_present_int() {
    // 1-element VariantArray (pvxs AnyA) carrying an int32 = 0x09.
    let desc = FieldDesc::VariantArray;
    let arr = PvField::VariantArray(vec![Some(VariantValue {
        desc: Some(FieldDesc::Scalar(ScalarType::Int)),
        value: PvField::Scalar(ScalarValue::Int(9)),
    })]);
    let mut out = Vec::new();
    encode_pv_field(&arr, &desc, ByteOrder::Big, &mut out);
    // pvxs emits: Size(1) + presence(0x01) + type_code(0x22 = Int) + Int(9) BE.
    assert_eq!(hex(&out), golden("variant_array_present_int"));
}
