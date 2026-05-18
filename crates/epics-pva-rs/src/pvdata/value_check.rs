//! PVA-R5 + PVA-R9 + PVA-R16 helper: check that a [`PvField`] value
//! structurally matches a [`FieldDesc`].
//!
//! pvxs `dataencode.cpp` is descriptor-driven: every wire encode uses
//! the FieldDesc tree the channel was opened with, and the typed value
//! arms assume the runtime value matches that descriptor at every
//! level (`assert(Value::Helper::desc(elem) == &desc->members[0])`).
//! When a producer hands the server a descriptor-mismatched value,
//! pvxs throws and turns the mismatch into a wire-level operation
//! error.
//!
//! Rust pre-fix routed descriptor-mismatched value/desc pairs through
//! `pvdata::encode::encode_pv_field`'s "(F-G10) Generic fallback"
//! which silently emits a default/coerced wire shape under the
//! advertised descriptor. That converts an upstream producer bug
//! into a valid-looking PVA response — exactly what PVA-R5 / PVA-R9
//! flag.
//!
//! The check here is a top-level structural match: PvField variant
//! must align with the FieldDesc variant, and for compound types
//! either the structure id / fields must match or the variant
//! descriptor must align. We deliberately do NOT walk all children
//! — a deep recursive walk is expensive on a per-post hot path, and
//! the encoder's typed arms already crash safely on grandchild
//! mismatch (the F-G10 fallback only fires for the top-level pair).
//! Catching the most common producer mistake — opening one
//! descriptor and posting an unrelated value — is sufficient for
//! PVA-R5/R9 parity with pvxs's outer throw.

use super::{FieldDesc, PvField, ScalarValue};

/// Reason a value does not fit a descriptor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueDescMismatch {
    #[error("descriptor type {desc} does not match value variant {value}")]
    VariantMismatch {
        desc: &'static str,
        value: &'static str,
    },
    #[error("scalar type {desc:?} does not match value scalar type {value:?}")]
    ScalarTypeMismatch {
        desc: super::ScalarType,
        value: super::ScalarType,
    },
    #[error("structure id mismatch: descriptor `{desc_id}`, value `{value_id}`")]
    StructureIdMismatch { desc_id: String, value_id: String },
}

/// True iff `value` can be encoded under `desc` without taking the
/// `encode::encode_pv_field` "F-G10 generic fallback" silent-coerce
/// path. Used by the server wire layer (and `SharedPV::try_post`) to
/// reject producer-side descriptor/value mismatches before the bytes
/// hit the wire — matching pvxs' "throw on mismatch" contract.
pub fn value_matches_descriptor(
    value: &PvField,
    desc: &FieldDesc,
) -> Result<(), ValueDescMismatch> {
    match (value, desc) {
        // Scalar
        (PvField::Scalar(v), FieldDesc::Scalar(t)) => {
            let v_ty = scalar_type_of(v);
            if v_ty == *t {
                Ok(())
            } else {
                Err(ValueDescMismatch::ScalarTypeMismatch {
                    desc: *t,
                    value: v_ty,
                })
            }
        }
        // Scalar arrays (either enum-tagged or typed variant).
        (PvField::ScalarArray(_), FieldDesc::ScalarArray(_))
        | (PvField::ScalarArrayTyped(_), FieldDesc::ScalarArray(_)) => Ok(()),
        // Structure: compare struct_id only (member walk is expensive
        // and the encoder already handles per-member coercion for
        // descriptor-named fields via `get_field(name)` lookups).
        (PvField::Structure(s), FieldDesc::Structure { struct_id, .. }) => {
            if &s.struct_id == struct_id {
                Ok(())
            } else {
                Err(ValueDescMismatch::StructureIdMismatch {
                    desc_id: struct_id.clone(),
                    value_id: s.struct_id.clone(),
                })
            }
        }
        (PvField::StructureArray(_), FieldDesc::StructureArray { .. }) => Ok(()),
        // Union — the descriptor lists variants; the value just has
        // a selector. Selector range is enforced at encode time.
        (PvField::Union { .. }, FieldDesc::Union { .. }) => Ok(()),
        (PvField::UnionArray(_), FieldDesc::UnionArray { .. }) => Ok(()),
        // Variant / VariantArray — pvxs Any/AnyA accepts any
        // payload because the value carries its own descriptor.
        (PvField::Variant(_), FieldDesc::Variant) => Ok(()),
        (PvField::VariantArray(_), FieldDesc::VariantArray) => Ok(()),
        // Bounded string ≈ Scalar(String) on the value side.
        (PvField::Scalar(ScalarValue::String(_)), FieldDesc::BoundedString(_)) => Ok(()),
        // Null value with any descriptor: allowed only for an
        // unspecified Variant slot (pvxs writes 0xFF for null Any).
        (PvField::Null, FieldDesc::Variant) => Ok(()),
        (val, desc) => Err(ValueDescMismatch::VariantMismatch {
            desc: desc_label(desc),
            value: value_label(val),
        }),
    }
}

fn scalar_type_of(v: &ScalarValue) -> super::ScalarType {
    use super::ScalarType as S;
    match v {
        ScalarValue::Boolean(_) => S::Boolean,
        ScalarValue::Byte(_) => S::Byte,
        ScalarValue::UByte(_) => S::UByte,
        ScalarValue::Short(_) => S::Short,
        ScalarValue::UShort(_) => S::UShort,
        ScalarValue::Int(_) => S::Int,
        ScalarValue::UInt(_) => S::UInt,
        ScalarValue::Long(_) => S::Long,
        ScalarValue::ULong(_) => S::ULong,
        ScalarValue::Float(_) => S::Float,
        ScalarValue::Double(_) => S::Double,
        ScalarValue::String(_) => S::String,
    }
}

fn value_label(v: &PvField) -> &'static str {
    match v {
        PvField::Scalar(_) => "Scalar",
        PvField::ScalarArray(_) => "ScalarArray",
        PvField::ScalarArrayTyped(_) => "ScalarArrayTyped",
        PvField::Structure(_) => "Structure",
        PvField::StructureArray(_) => "StructureArray",
        PvField::Union { .. } => "Union",
        PvField::UnionArray(_) => "UnionArray",
        PvField::Variant(_) => "Variant",
        PvField::VariantArray(_) => "VariantArray",
        PvField::Null => "Null",
    }
}

fn desc_label(d: &FieldDesc) -> &'static str {
    match d {
        FieldDesc::Scalar(_) => "Scalar",
        FieldDesc::ScalarArray(_) => "ScalarArray",
        FieldDesc::Structure { .. } => "Structure",
        FieldDesc::StructureArray { .. } => "StructureArray",
        FieldDesc::Union { .. } => "Union",
        FieldDesc::UnionArray { .. } => "UnionArray",
        FieldDesc::Variant => "Variant",
        FieldDesc::VariantArray => "VariantArray",
        FieldDesc::BoundedString(_) => "BoundedString",
    }
}
