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
//! decoders that drop nested children or mis-read the variant tag —
//! without requiring `FieldDesc` to implement `PartialEq`.

use std::io::Cursor;

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::{
    BoundedStringPolicy, TypeCache, decode_type_desc, decode_type_desc_cached_with_policy,
    encode_type_desc,
};
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

/// Regression R0604-PVDATA-BOUNDED-STRING-PVACCESSCPP-1.
///
/// EPICS base pvData serializes a `BoundedString` as `0x83` followed by
/// `SerializeHelper::writeSize(maxLength)` (`FieldCreateFactory.cpp:201-206,
/// 1512-1523`), and pvAccessCPP servers put it on the wire. pvxs has no
/// bounded-string TypeCode and faults the descriptor on decode
/// (`dataencode.cpp:120-123,186-206`). The two dialects disagree, so the
/// behaviour is policy-driven:
///
/// - default [`BoundedStringPolicy::Interop`]: consume the `maxLength` size
///   word and surface the field as a plain `String`, so GET/MONITOR/PUT work
///   against Base pvAccessCPP servers. `FieldDesc` carries no bound, so
///   re-encoding normalizes back to the plain `0x60` String tag toward pvxs.
/// - explicit [`BoundedStringPolicy::StrictPvxs`]: reject `0x83` (also rejects
///   the legacy Rust-only `0x83` that pre-interop builds emitted).
#[test]
fn bounded_string_wire_tag_interop_decodes_strict_rejects() {
    for order in [ByteOrder::Big, ByteOrder::Little] {
        let bytes = [0x83u8, 0x40]; // tag + writeSize(64) (one-byte size word)

        // Default entry point == Interop: decodes to a plain String scalar and
        // consumes the maxLength size word.
        let mut cur = Cursor::new(bytes.as_slice());
        let desc = decode_type_desc(&mut cur, order).unwrap_or_else(|e| {
            panic!("interop must decode 0x83 bounded string ({order:?}): {e:?}")
        });
        assert!(
            matches!(desc, FieldDesc::Scalar(ScalarType::String)),
            "interop bounded string must decode as plain String, got {desc:?} ({order:?})"
        );
        assert_eq!(
            cur.position(),
            2,
            "the maxLength size word must be consumed ({order:?})"
        );

        // Explicit pvxs-strict opt-in still rejects the tag.
        let mut cur = Cursor::new(bytes.as_slice());
        let mut cache = TypeCache::new();
        assert!(
            decode_type_desc_cached_with_policy(
                &mut cur,
                order,
                &mut cache,
                BoundedStringPolicy::StrictPvxs
            )
            .is_err(),
            "strict pvxs must reject the 0x83 bounded-string tag ({order:?})"
        );
    }
}

/// Regression R0604-PVDATA-BOUNDED-STRING-PVACCESSCPP-1 (nested + normalize).
///
/// A bounded string nested inside a structure must decode under the default
/// interop policy — proving the policy threads through the recursive descent
/// — and re-encoding must emit the plain `0x60` String tag, never `0x83`.
#[test]
fn bounded_string_nested_in_structure_interop_normalizes_to_plain_string() {
    for order in [ByteOrder::Big, ByteOrder::Little] {
        // Canonical pvxs form: a structure whose single field is a plain
        // String scalar. The encoder ends this descriptor with the `0x60`
        // String scalar tag.
        let canonical_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("s".into(), FieldDesc::Scalar(ScalarType::String))],
        };
        let mut canonical = Vec::new();
        encode_type_desc(&canonical_desc, order, &mut canonical);
        assert_eq!(
            *canonical.last().expect("non-empty descriptor"),
            0x60,
            "canonical structure must end in the 0x60 String scalar tag ({order:?})"
        );

        // EPICS base pvAccessCPP form: same structure header, but the field's
        // String tag is replaced by a bounded-string descriptor
        // (`0x83` + writeSize(64)).
        let mut base_wire = canonical.clone();
        base_wire.pop(); // drop the trailing 0x60 String tag
        base_wire.push(0x83); // bounded-string tag
        base_wire.push(0x40); // writeSize(64) -- one-byte maxLength

        // Interop decodes the nested bounded string (recursion threading), and
        // re-encoding normalizes back to the canonical 0x60 form.
        let mut cur = Cursor::new(base_wire.as_slice());
        let decoded = decode_type_desc(&mut cur, order).unwrap_or_else(|e| {
            panic!("interop must decode nested bounded string ({order:?}): {e:?}")
        });
        let mut reencoded = Vec::new();
        encode_type_desc(&decoded, order, &mut reencoded);
        assert_eq!(
            reencoded, canonical,
            "nested bounded string must re-encode to the canonical 0x60 form ({order:?})"
        );
        assert!(
            !reencoded.contains(&0x83),
            "re-encoded descriptor must not emit the 0x83 bounded-string tag ({order:?})"
        );
    }
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
