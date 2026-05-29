//! Check that a [`PvField`] value
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
//! `pvdata::encode::encode_pv_field`'s "Generic fallback"
//! which silently emits a default/coerced wire shape under the
//! advertised descriptor. That converts an upstream producer bug
//! into a valid-looking PVA response — exactly the descriptor
//! mismatch this check rejects.
//!
//! The check walks the value and descriptor trees together, mirroring
//! the encoder's coercion surface: at every leaf where the encoder
//! would silently coerce a mismatched scalar (`encode_pv_field` /
//! `encode_pv_field_generic`) or empty/retype a mismatched scalar
//! array, this function returns an error instead so the mismatch is
//! turned into a wire-level operation error (pvxs' outer throw).
//! Compound types (`Structure`, `StructureArray`, `Union`,
//! `UnionArray`) are recursed so a leaf mismatch nested under a
//! matching outer shape — e.g. `value: Int` posted under an
//! `NTScalar<Double>` descriptor that shares the same `struct_id` — is
//! caught rather than coerced. An earlier revision compared only the
//! outer shape (`struct_id` for a structure, length for an array) and
//! let nested leaf mismatches reach the coercing fallback; that was
//! the defect this closes. `Variant`/`VariantArray` are accepted as-is
//! because the value carries its own descriptor (the encoder emits it
//! inline rather than coercing to a fixed type); a `Null` field
//! encodes as the descriptor's default and likewise carries no
//! value-derived data to corrupt.

use super::{FieldDesc, PvField, PvStructure, ScalarValue};

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
    #[error("union selector {selector} out of range ({variants} variant(s))")]
    UnionSelectorOutOfRange { selector: i32, variants: usize },
}

/// True iff `value` can be encoded under `desc` without taking the
/// `encode::encode_pv_field` "generic fallback" silent-coerce
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
        // Scalar arrays: the element type must equal the descriptor's.
        // A mismatch routes through `encode_pv_field_generic`, which
        // coerces every element to the descriptor type (or empties the
        // array) — silent corruption, e.g. a `uint[]` posted under an
        // `int[]` descriptor. An empty enum-tagged array carries no
        // element type and so always fits.
        (PvField::ScalarArray(items), FieldDesc::ScalarArray(st)) => {
            match items.iter().map(scalar_type_of).find(|t| t != st) {
                Some(value) => Err(ValueDescMismatch::ScalarTypeMismatch { desc: *st, value }),
                None => Ok(()),
            }
        }
        (PvField::ScalarArrayTyped(arr), FieldDesc::ScalarArray(st)) => {
            if arr.scalar_type() == *st {
                Ok(())
            } else {
                Err(ValueDescMismatch::ScalarTypeMismatch {
                    desc: *st,
                    value: arr.scalar_type(),
                })
            }
        }
        // Structure: `struct_id` must match AND every value field the
        // encoder will emit must fit its descriptor. The encoder
        // recurses per descriptor-named field (`encode_pv_field`'s
        // Structure arm), coercing leaf mismatches; matching only
        // `struct_id` let `value: Int` ride out under an
        // `NTScalar<Double>` descriptor of the same id.
        (PvField::Structure(s), FieldDesc::Structure { struct_id, fields }) => {
            if &s.struct_id != struct_id {
                return Err(ValueDescMismatch::StructureIdMismatch {
                    desc_id: struct_id.clone(),
                    value_id: s.struct_id.clone(),
                });
            }
            structure_fields_match(s, fields)
        }
        // Structure array: every present element must carry the
        // descriptor's element `struct_id`, then match its field list.
        // pvxs encodes a present StructA element only after asserting the
        // element value's descriptor IS the array element descriptor
        // (dataencode.cpp:354-365), and decodes each element under that
        // descriptor's id (dataencode.cpp:607-618) — the element body
        // carries no id of its own. Checking only the field list let an
        // `other_t` element ride out under a `row_t[]` descriptor: local
        // diagnostics see `other_t`, every peer decodes `row_t`. A `None`
        // (absent) element emits a presence byte only and cannot corrupt.
        (PvField::StructureArray(items), FieldDesc::StructureArray { struct_id, fields }) => {
            for elem in items.iter().flatten() {
                if &elem.struct_id != struct_id {
                    return Err(ValueDescMismatch::StructureIdMismatch {
                        desc_id: struct_id.clone(),
                        value_id: elem.struct_id.clone(),
                    });
                }
                structure_fields_match(elem, fields)?;
            }
            Ok(())
        }
        // Union: a null selector (`< 0`) encodes as the 0xFF null marker
        // and cannot corrupt. An in-range selector picks a variant whose
        // held value must match. An out-of-range selector is a producer
        // bug the encoder masks as a null marker — reject it here so a
        // checked server reply / SharedPV post cannot silently send null.
        (
            PvField::Union {
                selector, value, ..
            },
            FieldDesc::Union { variants, .. },
        ) => check_union_selector(*selector, value, variants),
        (PvField::UnionArray(items), FieldDesc::UnionArray { variants, .. }) => {
            for it in items.iter().flatten() {
                check_union_selector(it.selector, &it.value, variants)?;
            }
            Ok(())
        }
        // Variant / VariantArray — pvxs Any/AnyA accepts any
        // payload because the value carries its own descriptor.
        (PvField::Variant(_), FieldDesc::Variant) => Ok(()),
        (PvField::VariantArray(_), FieldDesc::VariantArray) => Ok(()),
        // Null value with any descriptor: allowed only for an
        // unspecified Variant slot (pvxs writes 0xFF for null Any).
        (PvField::Null, FieldDesc::Variant) => Ok(()),
        (val, desc) => Err(ValueDescMismatch::VariantMismatch {
            desc: desc_label(desc),
            value: value_label(val),
        }),
    }
}

/// Check every value field the encoder will emit for `s` against its
/// matching descriptor field. `encode_pv_field`'s Structure arm emits
/// each descriptor-named field from `s` when present, else a type
/// default; only a *present* field can carry a coerced mismatch, so
/// absent fields are not walked (a partial post / monitor delta is not
/// a mismatch).
fn structure_fields_match(
    s: &PvStructure,
    fields: &[(String, FieldDesc)],
) -> Result<(), ValueDescMismatch> {
    for (name, child_desc) in fields {
        if let Some(child_val) = s.get_field(name) {
            value_matches_descriptor_child(child_val, child_desc)?;
        }
    }
    Ok(())
}

/// Recurse into a nested field. A `Null` child encodes as the
/// descriptor's default (no value-derived data), so it fits any
/// descriptor in a nested position — unlike a top-level `Null`, which
/// fits only a `Variant` slot.
fn value_matches_descriptor_child(
    value: &PvField,
    desc: &FieldDesc,
) -> Result<(), ValueDescMismatch> {
    if matches!(value, PvField::Null) {
        return Ok(());
    }
    value_matches_descriptor(value, desc)
}

/// Validate a union selector and, when it selects a variant, the held
/// value against that variant's descriptor.
///
/// pvxs decode treats the null selector (`-1` Size sentinel) as null but
/// FAULTS an out-of-range selector — `dataencode.cpp:520-538` for `Union`
/// and `:624-650` for present `UnionA` elements. A locally-built union
/// whose selector is past the variant list is a producer bug pvxs would
/// never put on the wire; the Rust encoder masks it as the 0xFF null
/// marker, so it must be rejected here before a checked path emits it.
fn check_union_selector(
    selector: i32,
    value: &PvField,
    variants: &[(String, FieldDesc)],
) -> Result<(), ValueDescMismatch> {
    if selector < 0 {
        return Ok(()); // null union — the 0xFF marker, no value bytes
    }
    match variants.get(selector as usize) {
        Some((_, vdesc)) => value_matches_descriptor_child(value, vdesc),
        None => Err(ValueDescMismatch::UnionSelectorOutOfRange {
            selector,
            variants: variants.len(),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::{ScalarType, UnionItem, VariantValue};

    const NT_SCALAR: &str = "epics:nt/NTScalar:1.0";

    fn nt_scalar_desc(value_ty: ScalarType) -> FieldDesc {
        FieldDesc::Structure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(value_ty))],
        }
    }

    fn nt_scalar_value(value: ScalarValue) -> PvField {
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Scalar(value));
        PvField::Structure(s)
    }

    // ── The cited HIGH defect: a leaf type mismatch nested under a
    //    matching outer NTScalar struct_id. ──────────────────────────

    #[test]
    fn nested_scalar_type_mismatch_in_structure_is_err() {
        // value: Int under an NTScalar<Double> descriptor of the same id.
        let desc = nt_scalar_desc(ScalarType::Double);
        let value = nt_scalar_value(ScalarValue::Int(7));
        assert!(
            matches!(
                value_matches_descriptor(&value, &desc),
                Err(ValueDescMismatch::ScalarTypeMismatch {
                    desc: ScalarType::Double,
                    value: ScalarType::Int,
                })
            ),
            "value: Int must not pass an NTScalar<Double> descriptor"
        );
    }

    #[test]
    fn nested_scalar_type_match_in_structure_is_ok() {
        let desc = nt_scalar_desc(ScalarType::Double);
        let value = nt_scalar_value(ScalarValue::Double(1.5));
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    #[test]
    fn struct_id_mismatch_is_err() {
        let desc = nt_scalar_desc(ScalarType::Double);
        let mut s = PvStructure::new("epics:nt/NTEnum:1.0");
        s.set("value", PvField::Scalar(ScalarValue::Double(1.0)));
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::StructureIdMismatch { .. })
        ));
    }

    #[test]
    fn partial_structure_missing_field_is_ok() {
        // Descriptor has `value` + `alarm`; value posts only `value`.
        // The absent field encodes as a default — a partial post / delta
        // is not a mismatch.
        let desc = FieldDesc::Structure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![
                ("value".to_string(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".to_string(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".to_string(),
                        fields: vec![("severity".to_string(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
            ],
        };
        let value = nt_scalar_value(ScalarValue::Double(2.0));
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    #[test]
    fn null_nested_field_fits_any_descriptor() {
        // A present-but-Null field encodes as the descriptor default.
        let desc = nt_scalar_desc(ScalarType::Double);
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Null);
        assert!(value_matches_descriptor(&PvField::Structure(s), &desc).is_ok());
    }

    // ── Scalar-array element-type boundary. ─────────────────────────

    #[test]
    fn enum_tagged_array_element_type_mismatch_is_err() {
        let desc = FieldDesc::ScalarArray(ScalarType::Int);
        let value = PvField::ScalarArray(vec![ScalarValue::UInt(1), ScalarValue::UInt(2)]);
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::ScalarTypeMismatch {
                desc: ScalarType::Int,
                value: ScalarType::UInt,
            })
        ));
    }

    #[test]
    fn empty_enum_tagged_array_fits_any_element_type() {
        let desc = FieldDesc::ScalarArray(ScalarType::Int);
        let value = PvField::ScalarArray(Vec::new());
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    #[test]
    fn typed_array_wrong_element_type_is_err() {
        let desc = FieldDesc::ScalarArray(ScalarType::Double);
        let value = PvField::scalar_array_int(vec![1, 2, 3]);
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::ScalarTypeMismatch {
                desc: ScalarType::Double,
                value: ScalarType::Int,
            })
        ));
    }

    #[test]
    fn typed_array_matching_element_type_is_ok() {
        let desc = FieldDesc::ScalarArray(ScalarType::Double);
        let value = PvField::scalar_array_double(vec![1.0, 2.0]);
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    // ── Compound recursion: StructureArray + Union. ─────────────────

    #[test]
    fn structure_array_element_nested_mismatch_is_err() {
        let fields = vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))];
        let desc = FieldDesc::StructureArray {
            struct_id: "row_t".to_string(),
            fields: fields.clone(),
        };
        let mut elem = PvStructure::new("row_t");
        elem.set("value", PvField::Scalar(ScalarValue::Int(1)));
        let value = PvField::StructureArray(vec![Some(elem)]);
        assert!(value_matches_descriptor(&value, &desc).is_err());
    }

    #[test]
    fn structure_array_element_struct_id_mismatch_is_err() {
        // Descriptor is `row_t[]`; the element has identical fields but a
        // different `struct_id` (`other_t`). Field-only checking accepted
        // it; the element id must be compared too.
        let fields = vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Int))];
        let desc = FieldDesc::StructureArray {
            struct_id: "row_t".to_string(),
            fields: fields.clone(),
        };
        let mut elem = PvStructure::new("other_t");
        elem.set("value", PvField::Scalar(ScalarValue::Int(1)));
        let value = PvField::StructureArray(vec![Some(elem)]);
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::StructureIdMismatch {
                ref desc_id,
                ref value_id,
            }) if desc_id == "row_t" && value_id == "other_t"
        ));
    }

    #[test]
    fn structure_array_element_matching_struct_id_is_ok() {
        let fields = vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Int))];
        let desc = FieldDesc::StructureArray {
            struct_id: "row_t".to_string(),
            fields: fields.clone(),
        };
        let mut elem = PvStructure::new("row_t");
        elem.set("value", PvField::Scalar(ScalarValue::Int(1)));
        let value = PvField::StructureArray(vec![Some(elem)]);
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    fn double_or_int_union_desc() -> FieldDesc {
        FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("d".to_string(), FieldDesc::Scalar(ScalarType::Double)),
                ("i".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        }
    }

    #[test]
    fn union_selected_variant_type_mismatch_is_err() {
        // selector 0 picks the Double variant; value is Int → coercion.
        let desc = double_or_int_union_desc();
        let value = PvField::Union {
            selector: 0,
            variant_name: "d".to_string(),
            value: Box::new(PvField::Scalar(ScalarValue::Int(1))),
        };
        assert!(value_matches_descriptor(&value, &desc).is_err());
    }

    #[test]
    fn union_null_selector_is_ok() {
        let desc = double_or_int_union_desc();
        let value = PvField::Union {
            selector: -1,
            variant_name: String::new(),
            value: Box::new(PvField::Null),
        };
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }

    #[test]
    fn union_out_of_range_selector_is_err() {
        // selector == variants.len() is past the last variant (only 0,1
        // are valid for a 2-variant union).
        let desc = double_or_int_union_desc();
        let value = PvField::Union {
            selector: 2,
            variant_name: String::new(),
            value: Box::new(PvField::Scalar(ScalarValue::Double(1.0))),
        };
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::UnionSelectorOutOfRange {
                selector: 2,
                variants: 2,
            })
        ));
    }

    #[test]
    fn union_array_out_of_range_selector_is_err() {
        let desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![("d".to_string(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let value = PvField::UnionArray(vec![Some(UnionItem {
            selector: 5,
            variant_name: String::new(),
            value: PvField::Scalar(ScalarValue::Double(1.0)),
        })]);
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::UnionSelectorOutOfRange { selector: 5, .. })
        ));
    }

    #[test]
    fn union_array_element_variant_mismatch_is_err() {
        let desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![
                ("d".to_string(), FieldDesc::Scalar(ScalarType::Double)),
                ("i".to_string(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let value = PvField::UnionArray(vec![Some(UnionItem {
            selector: 0,
            variant_name: "d".to_string(),
            value: PvField::Scalar(ScalarValue::Int(9)),
        })]);
        assert!(value_matches_descriptor(&value, &desc).is_err());
    }

    #[test]
    fn variant_accepts_any_payload() {
        // Variant carries its own descriptor; the encoder emits it
        // inline, so any payload fits.
        let desc = FieldDesc::Variant;
        let value = PvField::Variant(Box::new(VariantValue {
            desc: Some(FieldDesc::Scalar(ScalarType::Int)),
            value: PvField::Scalar(ScalarValue::Int(3)),
        }));
        assert!(value_matches_descriptor(&value, &desc).is_ok());
    }
}
