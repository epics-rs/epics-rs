//! `FieldDesc` (type descriptor) byte-stable round-trip — pvxs's
//! `to_wire(TypeCode)` / `from_wire(TypeCode)` parity, exercised
//! through every `FieldDesc` variant.
//!
//! pvxs reference: `src/dataencode.cpp` `to_wire`/`from_wire` for
//! `TypeCode` and the typeCodeLUT in `FieldCreateFactory.cpp`. The
//! Rust encoder mirrors pvxs's tags (Scalar = 0x00..0x60,
//! ScalarArray = scalar | 0x08, Structure / Union / *Array =
//! their distinct `TAG_*` bytes).
//!
//! The test pattern is byte-stable round-trip: encode → decode →
//! re-encode and assert the byte streams match. This catches
//! decoders that drop nested children, mis-read the variant tag,
//! or skip `BoundedString`'s 4-byte bound — without requiring
//! `FieldDesc` to implement `PartialEq`.

use std::io::Cursor;

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::{decode_type_desc, encode_type_desc};
use epics_pva_rs::pvdata::{FieldDesc, ScalarType};

fn round_trip(desc: &FieldDesc, order: ByteOrder) {
    let mut once = Vec::new();
    encode_type_desc(desc, order, &mut once);
    let mut cur = Cursor::new(once.as_slice());
    let decoded = decode_type_desc(&mut cur, order)
        .unwrap_or_else(|e| panic!("decode failed for {desc:?} ({order:?}): {e:?}"));
    let mut twice = Vec::new();
    encode_type_desc(&decoded, order, &mut twice);
    assert_eq!(
        once, twice,
        "round-trip bytes diverged for {desc:?} ({order:?})"
    );
}

fn both_orders(desc: FieldDesc) {
    round_trip(&desc, ByteOrder::Big);
    round_trip(&desc, ByteOrder::Little);
}

const ALL_SCALARS: &[ScalarType] = &[
    ScalarType::Boolean,
    ScalarType::Byte,
    ScalarType::Short,
    ScalarType::Int,
    ScalarType::Long,
    ScalarType::UByte,
    ScalarType::UShort,
    ScalarType::UInt,
    ScalarType::ULong,
    ScalarType::Float,
    ScalarType::Double,
    ScalarType::String,
];

#[test]
fn type_code_roundtrip_all_scalars() {
    for &st in ALL_SCALARS {
        both_orders(FieldDesc::Scalar(st));
    }
}

#[test]
fn type_code_roundtrip_all_scalar_arrays() {
    // Array tag = scalar tag | 0x08; all 12 scalar element types.
    for &st in ALL_SCALARS {
        both_orders(FieldDesc::ScalarArray(st));
    }
}

#[test]
fn type_code_roundtrip_structure_named_with_id() {
    let d = FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("descriptor".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    };
    both_orders(d);
}

#[test]
fn type_code_roundtrip_structure_empty_id() {
    // No struct_id (anonymous structure) — pvxs writes empty string,
    // decode must yield the same empty id.
    let d = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("v".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    both_orders(d);
}

#[test]
fn type_code_roundtrip_structure_array() {
    let d = FieldDesc::StructureArray {
        struct_id: String::new(),
        fields: vec![("v".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    both_orders(d);
}

#[test]
fn type_code_roundtrip_union_two_variants() {
    let d = FieldDesc::Union {
        struct_id: "any:1.0".into(),
        variants: vec![
            ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("d".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    };
    both_orders(d);
}

#[test]
fn type_code_roundtrip_union_array() {
    let d = FieldDesc::UnionArray {
        struct_id: String::new(),
        variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    both_orders(d);
}

#[test]
fn type_code_roundtrip_variant() {
    both_orders(FieldDesc::Variant);
}

#[test]
fn type_code_roundtrip_variant_array() {
    both_orders(FieldDesc::VariantArray);
}

#[test]
fn type_code_roundtrip_bounded_string() {
    // `BoundedString(N)` carries the 32-bit bound on the wire; a
    // decoder that silently truncates to a plain `String` would
    // re-encode as `String` and the byte streams would diverge.
    both_orders(FieldDesc::BoundedString(64));
    both_orders(FieldDesc::BoundedString(0));
    both_orders(FieldDesc::BoundedString(u32::MAX));
}

#[test]
fn type_code_roundtrip_nested_structure() {
    // Structure containing a structure-array containing a structure —
    // exercises the recursive descent in `encode_structure_body`.
    let inner = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("x".into(), FieldDesc::Scalar(ScalarType::Int))],
    };
    let middle = FieldDesc::StructureArray {
        struct_id: String::new(),
        fields: vec![("inner".into(), inner)],
    };
    let outer = FieldDesc::Structure {
        struct_id: "outer:1.0".into(),
        fields: vec![
            ("seq".into(), FieldDesc::Scalar(ScalarType::ULong)),
            ("payload".into(), middle),
        ],
    };
    both_orders(outer);
}
