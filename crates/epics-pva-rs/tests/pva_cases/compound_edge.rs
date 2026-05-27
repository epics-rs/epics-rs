//! Compound-array per-element edges that the "two present" goldens
//! in `compound_arrays.rs` leave uncovered: null (absent) elements and
//! present/null mixes.
//!
//! pvxs reference: `src/dataencode.cpp` StructA/UnionA/AnyA encode
//! (`:354-393`). Per element the wire begins with a presence byte:
//! `0x00` = null (no body follows); `0x01` = present (body follows).
//!
//! `PvField::{StructureArray,UnionArray,VariantArray}` now
//! hold `Vec<Option<_>>`, so a `None` element encodes as pvxs's `0x00`
//! null shape — distinct from a *present* element whose body carries an
//! inner null sentinel. The expected bytes are the libpvxs captures in
//! `tools/pvxs-golden-capture/fixtures.txt` (read via `golden(...)`);
//! the Rust side only constructs the matching value and asserts the
//! encoder reproduces the captured bytes.

use std::io::Cursor;

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::{decode_pv_field, encode_pv_field};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `struct { int32 v }` element with the given value.
fn int_struct(v: i32) -> PvStructure {
    PvStructure {
        struct_id: String::new(),
        fields: vec![("v".to_string(), PvField::Scalar(ScalarValue::Int(v)))],
    }
}

fn struct_array_desc() -> FieldDesc {
    FieldDesc::StructureArray {
        struct_id: String::new(),
        fields: vec![("v".to_string(), FieldDesc::Scalar(ScalarType::Int))],
    }
}

#[test]
fn golden_pvxs_union_array_empty() {
    // The zero-element case: Size(0) only, no per-element bytes.
    let desc = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let val = PvField::UnionArray(vec![]);
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("union_array_empty"));
}

#[test]
fn golden_pvxs_struct_array_all_null() {
    // 3-element struct[] with every element null → Size(3) + 0x00 x3.
    let val = PvField::StructureArray(vec![None, None, None]);
    let mut out = Vec::new();
    encode_pv_field(&val, &struct_array_desc(), ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("struct_array_all_null"));
}

#[test]
fn golden_pvxs_struct_array_present_null_present() {
    // [present{v=1}, null, present{v=3}].
    let val = PvField::StructureArray(vec![Some(int_struct(1)), None, Some(int_struct(3))]);
    let mut out = Vec::new();
    encode_pv_field(&val, &struct_array_desc(), ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("struct_array_present_null_present"));
}

#[test]
fn golden_pvxs_union_array_null_element() {
    // 1-element union[] with the lone element null.
    let desc = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let val = PvField::UnionArray(vec![None]);
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("union_array_null_element"));
}

#[test]
fn golden_pvxs_variant_array_null_descriptor() {
    // 1-element any[] with the lone element null.
    let desc = FieldDesc::VariantArray;
    let val = PvField::VariantArray(vec![None]);
    let mut out = Vec::new();
    encode_pv_field(&val, &desc, ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("variant_array_null_descriptor"));
}

/// Identity: decoding the libpvxs `present, null, present`
/// capture must recover `Some/None/Some` (not collapse the null into an
/// empty struct), and re-encoding must reproduce the captured bytes.
#[test]
fn struct_array_present_null_present_decode_roundtrip() {
    let bytes = unhex(golden("struct_array_present_null_present"));
    let mut cur = Cursor::new(bytes.as_slice());
    let decoded = decode_pv_field(&struct_array_desc(), &mut cur, ByteOrder::Big)
        .expect("decode present/null/present");
    match &decoded {
        PvField::StructureArray(items) => {
            assert_eq!(items.len(), 3);
            assert!(items[0].is_some(), "element 0 present");
            assert!(
                items[1].is_none(),
                "element 1 is a null element, not empty struct"
            );
            assert!(items[2].is_some(), "element 2 present");
            assert_eq!(
                items[0].as_ref().unwrap().get_field("v"),
                Some(&PvField::Scalar(ScalarValue::Int(1)))
            );
            assert_eq!(
                items[2].as_ref().unwrap().get_field("v"),
                Some(&PvField::Scalar(ScalarValue::Int(3)))
            );
        }
        other => panic!("expected StructureArray, got {other:?}"),
    }
    // Re-encode → byte-identical to the libpvxs capture.
    let mut out = Vec::new();
    encode_pv_field(&decoded, &struct_array_desc(), ByteOrder::Big, &mut out);
    assert_eq!(hex(&out), golden("struct_array_present_null_present"));
}
