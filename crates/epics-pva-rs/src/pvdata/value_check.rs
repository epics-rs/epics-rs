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
//! inline rather than coercing to a fixed type). A `Null` value fits
//! only a `Variant`/Any slot — at every level, top-level or nested —
//! where the wire is the 0xFF null marker; under any concrete
//! descriptor it is rejected. pvxs allocates typed storage per member
//! and has no nested-null concept (`data.cpp:96-121`), so a producer
//! that hands a concrete field a `Null` would have the encoder coerce
//! it to that descriptor's zero/default and pass fabricated data off
//! as real — exactly the coercion this check refuses.

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
    #[error("value is missing descriptor field `{name}`")]
    MissingField { name: String },
    #[error("value carries field `{name}` not present in the descriptor")]
    UnexpectedField { name: String },
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

/// Copy the marked subtrees of `delta` into `cur`, in place — pvxs
/// `Value::assign`, which is what `SharedPV::post` does to its stored
/// value (`sharedpv.cpp:431`). `marks` uses the wire bit numbering of
/// `desc` (root 0, depth-first): a set structure bit copies the whole
/// subtree, a set leaf bit copies that leaf, and an unmarked leaf of
/// `delta` is neither read nor checked. Every marked subtree is checked
/// against its descriptor ([`value_matches_descriptor`]) before anything
/// is copied, so on `Err` `cur` is untouched. A marked subtree that
/// `delta` lacks is `MissingField`; a non-structure where a marked bit
/// lies below is `VariantMismatch`. `cur` must fit `desc`, as an opened
/// value does; a marked child it lacks is appended.
pub fn apply_marked_delta(
    desc: &FieldDesc,
    marks: &crate::proto::BitSet,
    bit_offset: usize,
    delta: &PvField,
    cur: &mut PvField,
) -> Result<(), ValueDescMismatch> {
    check_marked(desc, marks, bit_offset, delta)?;
    copy_marked(desc, marks, bit_offset, delta, cur);
    Ok(())
}

fn check_marked(
    desc: &FieldDesc,
    marks: &crate::proto::BitSet,
    bit: usize,
    delta: &PvField,
) -> Result<(), ValueDescMismatch> {
    if marks.get(bit) {
        return value_matches_descriptor(delta, desc);
    }
    let FieldDesc::Structure { fields, .. } = desc else {
        return Ok(());
    };
    let marked_below = |start: usize, end: usize| (start..end).any(|b| marks.get(b));
    let PvField::Structure(s) = delta else {
        return if marked_below(bit + 1, bit + desc.total_bits()) {
            Err(ValueDescMismatch::VariantMismatch {
                desc: desc_label(desc),
                value: value_label(delta),
            })
        } else {
            Ok(())
        };
    };
    let mut child_bit = bit + 1;
    for (i, (name, child_desc)) in fields.iter().enumerate() {
        let span = child_desc.total_bits();
        match field_at(s, i, name) {
            Some(child) => check_marked(child_desc, marks, child_bit, child)?,
            None => {
                if marked_below(child_bit, child_bit + span) {
                    return Err(ValueDescMismatch::MissingField { name: name.clone() });
                }
            }
        }
        child_bit += span;
    }
    Ok(())
}

fn copy_marked(
    desc: &FieldDesc,
    marks: &crate::proto::BitSet,
    bit: usize,
    delta: &PvField,
    cur: &mut PvField,
) {
    if marks.get(bit) {
        cur.clone_from(delta);
        return;
    }
    let FieldDesc::Structure { fields, .. } = desc else {
        return;
    };
    // `check_marked` proved nothing is marked below a non-structure delta.
    let (PvField::Structure(ds), PvField::Structure(cs)) = (delta, cur) else {
        return;
    };
    let mut child_bit = bit + 1;
    for (i, (name, child_desc)) in fields.iter().enumerate() {
        if let Some(d) = field_at(ds, i, name) {
            match field_at_mut(cs, i, name) {
                Some(c) => copy_marked(child_desc, marks, child_bit, d, c),
                None => {
                    let mut c = d.clone();
                    copy_marked(child_desc, marks, child_bit, d, &mut c);
                    cs.fields.push((name.clone(), c));
                }
            }
        }
        child_bit += child_desc.total_bits();
    }
}

/// The field named `name`, trying position `i` first: a value built from
/// the descriptor keeps its order, so the lookup is O(1) on that path.
fn field_at<'a>(s: &'a PvStructure, i: usize, name: &str) -> Option<&'a PvField> {
    match s.fields.get(i) {
        Some((n, v)) if n == name => Some(v),
        _ => s.get_field(name),
    }
}

fn field_at_mut<'a>(s: &'a mut PvStructure, i: usize, name: &str) -> Option<&'a mut PvField> {
    let hit = matches!(s.fields.get(i), Some((n, _)) if n == name);
    if hit {
        s.fields.get_mut(i).map(|(_, v)| v)
    } else {
        s.get_field_mut(name)
    }
}

/// A full value must carry the descriptor's exact member set: every
/// descriptor field present (and fitting), and no field the descriptor
/// does not name.
///
/// pvxs allocates typed storage for every descriptor member when a
/// `Value` is built (`data.cpp:96-121`) and resolves field access only
/// through the descriptor's `mlookup` table (`data.cpp:827-852`), so a
/// posted value's member set is structurally the descriptor's — never a
/// subset or superset. `SharedPV::post` accepts only the exact opened
/// descriptor (`sharedpv.cpp:417-431`). An absent field would otherwise
/// be silently encoded as `default_value_for(child_desc)`
/// (`encode.rs` Structure arm) and shipped as if real; an extra field
/// is never visited by the encoder, so a peer can never receive it
/// while local diagnostics see data no PVA client gets.
///
/// This is the FULL-value contract. Partial PUT deltas are not checked
/// here: they ride a separate changed-BitSet representation and are
/// merged against the prior complete value (`fill_unmarked_from_prior`)
/// into a complete value *before* reaching this check — so a checked
/// path only ever validates a structurally-complete value.
fn structure_fields_match(
    s: &PvStructure,
    fields: &[(String, FieldDesc)],
) -> Result<(), ValueDescMismatch> {
    for (name, child_desc) in fields {
        match s.get_field(name) {
            Some(child_val) => value_matches_descriptor(child_val, child_desc)?,
            None => return Err(ValueDescMismatch::MissingField { name: name.clone() }),
        }
    }
    for (name, _) in &s.fields {
        if !fields.iter().any(|(n, _)| n == name) {
            return Err(ValueDescMismatch::UnexpectedField { name: name.clone() });
        }
    }
    Ok(())
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
        Some((_, vdesc)) => value_matches_descriptor(value, vdesc),
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

    fn value_and_alarm_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![
                ("value".to_string(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".to_string(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".to_string(),
                        fields: vec![
                            ("severity".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        }
    }

    fn full_alarm() -> PvField {
        let mut alarm = PvStructure::new("alarm_t");
        alarm.set("severity", PvField::Scalar(ScalarValue::Int(0)));
        alarm.set("status", PvField::Scalar(ScalarValue::Int(0)));
        PvField::Structure(alarm)
    }

    #[test]
    fn structure_missing_top_level_field_is_err() {
        // Descriptor has `value` + `alarm`; value posts only `value`. A
        // full value must carry the descriptor's whole member set — the
        // absent `alarm` would otherwise ship as a default subtree.
        let desc = value_and_alarm_desc();
        let value = nt_scalar_value(ScalarValue::Double(2.0));
        assert!(matches!(
            value_matches_descriptor(&value, &desc),
            Err(ValueDescMismatch::MissingField { ref name }) if name == "alarm"
        ));
    }

    #[test]
    fn structure_missing_nested_field_is_err() {
        // `alarm` is present but its `status` member is absent — the
        // strictness reaches every nesting level, not just the root.
        let desc = value_and_alarm_desc();
        let mut alarm = PvStructure::new("alarm_t");
        alarm.set("severity", PvField::Scalar(ScalarValue::Int(0)));
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Scalar(ScalarValue::Double(2.0)));
        s.set("alarm", PvField::Structure(alarm));
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::MissingField { ref name }) if name == "status"
        ));
    }

    #[test]
    fn structure_extra_field_is_err() {
        // An extra `debug` field the descriptor does not name: the
        // encoder never visits it, so no PVA peer can receive it.
        let desc = value_and_alarm_desc();
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Scalar(ScalarValue::Double(2.0)));
        s.set("alarm", full_alarm());
        s.set("debug", PvField::Scalar(ScalarValue::Int(1)));
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::UnexpectedField { ref name }) if name == "debug"
        ));
    }

    #[test]
    fn structure_exact_member_set_is_ok() {
        // Every descriptor field present, no extras → accepted.
        let desc = value_and_alarm_desc();
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Scalar(ScalarValue::Double(2.0)));
        s.set("alarm", full_alarm());
        assert!(value_matches_descriptor(&PvField::Structure(s), &desc).is_ok());
    }

    #[test]
    fn null_nested_field_under_concrete_descriptor_is_err() {
        // A present-but-Null field under a concrete (non-Variant)
        // descriptor would be coerced to that descriptor's default by the
        // encoder, fabricating a value the producer never set — reject it.
        let desc = nt_scalar_desc(ScalarType::Double);
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Null);
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::VariantMismatch {
                desc: "Scalar",
                value: "Null",
            })
        ));
    }

    #[test]
    fn null_nested_field_under_variant_descriptor_is_ok() {
        // A Null under a Variant/Any slot is the legitimate 0xFF null
        // marker — accepted at the nested level just as at the top level.
        let desc = FieldDesc::Structure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![("value".to_string(), FieldDesc::Variant)],
        };
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Null);
        assert!(value_matches_descriptor(&PvField::Structure(s), &desc).is_ok());
    }

    #[test]
    fn null_nested_field_under_scalar_array_descriptor_is_err() {
        // NTScalarArray<Int>.value = Null would coerce to an empty array.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::ScalarArray(ScalarType::Int))],
        };
        let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
        s.set("value", PvField::Null);
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::VariantMismatch {
                desc: "ScalarArray",
                value: "Null",
            })
        ));
    }

    #[test]
    fn null_deeply_nested_field_is_err() {
        // alarm.severity = Null, two levels down, must still be rejected —
        // the recursion reaches every concrete leaf, not just the top one.
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
        let mut alarm = PvStructure::new("alarm_t");
        alarm.set("severity", PvField::Null);
        let mut s = PvStructure::new(NT_SCALAR);
        s.set("value", PvField::Scalar(ScalarValue::Double(1.0)));
        s.set("alarm", PvField::Structure(alarm));
        assert!(matches!(
            value_matches_descriptor(&PvField::Structure(s), &desc),
            Err(ValueDescMismatch::VariantMismatch {
                desc: "Scalar",
                value: "Null",
            })
        ));
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

    /// { value: Int, alarm { severity: Int, message: String } } — bits: 0
    /// root, 1 value, 2 alarm, 3 severity, 4 message.
    fn alarmed_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![
                ("value".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                (
                    "alarm".to_string(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".to_string(),
                        fields: vec![
                            ("severity".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".to_string(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
            ],
        }
    }

    fn alarmed_value(value: ScalarValue, severity: ScalarValue, message: &str) -> PvField {
        PvField::Structure(PvStructure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![
                ("value".to_string(), PvField::Scalar(value)),
                (
                    "alarm".to_string(),
                    PvField::Structure(PvStructure {
                        struct_id: "alarm_t".to_string(),
                        fields: vec![
                            ("severity".to_string(), PvField::Scalar(severity)),
                            (
                                "message".to_string(),
                                PvField::Scalar(ScalarValue::String(message.into())),
                            ),
                        ],
                    }),
                ),
            ],
        })
    }

    fn bits(set: &[usize]) -> crate::proto::BitSet {
        let mut b = crate::proto::BitSet::new();
        for &i in set {
            b.set(i);
        }
        b
    }

    /// Per boundary of the mark test: a marked leaf is copied, an unmarked
    /// leaf is left alone even when `delta` differs there, and a marked
    /// structure bit copies its whole subtree.
    #[test]
    fn apply_marked_delta_copies_exactly_the_marked_subtrees() {
        let desc = alarmed_desc();
        let mut cur = alarmed_value(ScalarValue::Int(1), ScalarValue::Int(0), "ok");
        let delta = alarmed_value(ScalarValue::Int(2), ScalarValue::Int(3), "hi");

        apply_marked_delta(&desc, &bits(&[1]), 0, &delta, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(2), ScalarValue::Int(0), "ok"),
            "only the marked leaf moved"
        );

        apply_marked_delta(&desc, &bits(&[4]), 0, &delta, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(2), ScalarValue::Int(0), "hi"),
            "a nested marked leaf moved, its unmarked sibling did not"
        );

        apply_marked_delta(&desc, &bits(&[2]), 0, &delta, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(2), ScalarValue::Int(3), "hi"),
            "a marked structure bit copies the subtree"
        );

        let mut untouched = alarmed_value(ScalarValue::Int(1), ScalarValue::Int(0), "ok");
        apply_marked_delta(&desc, &bits(&[]), 0, &delta, &mut untouched).unwrap();
        assert_eq!(
            untouched,
            alarmed_value(ScalarValue::Int(1), ScalarValue::Int(0), "ok"),
            "nothing marked, nothing copied"
        );
    }

    /// The check covers only what is marked, and it runs before any copy:
    /// a mismatch on an unmarked leaf is invisible, a mismatch on a marked
    /// leaf refuses the whole delta with the earlier marked leaf still
    /// uncopied.
    #[test]
    fn apply_marked_delta_checks_marked_leaves_only_and_before_copying() {
        let desc = alarmed_desc();
        let before = alarmed_value(ScalarValue::Int(1), ScalarValue::Int(0), "ok");
        // `severity` carries a Double: wrong under the descriptor.
        let delta = alarmed_value(ScalarValue::Int(2), ScalarValue::Double(3.0), "hi");

        let mut cur = before.clone();
        apply_marked_delta(&desc, &bits(&[1]), 0, &delta, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(2), ScalarValue::Int(0), "ok"),
            "the unmarked bad leaf is not checked"
        );

        let mut cur = before.clone();
        let err = apply_marked_delta(&desc, &bits(&[1, 3]), 0, &delta, &mut cur).unwrap_err();
        assert!(
            matches!(err, ValueDescMismatch::ScalarTypeMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(cur, before, "a refused delta leaves the value untouched");

        // A marked structure bit checks the subtree it would copy.
        let mut cur = before.clone();
        assert!(apply_marked_delta(&desc, &bits(&[2]), 0, &delta, &mut cur).is_err());
        assert_eq!(cur, before);
    }

    /// A marked subtree the delta does not carry is `MissingField`; an
    /// absent unmarked one is fine. A marked bit below a delta node that is
    /// not a structure is `VariantMismatch`.
    #[test]
    fn apply_marked_delta_refuses_a_marked_subtree_the_delta_lacks() {
        let desc = alarmed_desc();
        let before = alarmed_value(ScalarValue::Int(1), ScalarValue::Int(0), "ok");
        // Only `value`; no `alarm` member at all.
        let delta = PvField::Structure(PvStructure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![("value".to_string(), PvField::Scalar(ScalarValue::Int(5)))],
        });

        let mut cur = before.clone();
        apply_marked_delta(&desc, &bits(&[1]), 0, &delta, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(5), ScalarValue::Int(0), "ok"),
            "an absent unmarked member is not needed"
        );

        let mut cur = before.clone();
        let err = apply_marked_delta(&desc, &bits(&[3]), 0, &delta, &mut cur).unwrap_err();
        assert_eq!(
            err,
            ValueDescMismatch::MissingField {
                name: "alarm".to_string()
            }
        );
        assert_eq!(cur, before);

        // `alarm` present but not a structure, with a leaf below it marked.
        let flat = PvField::Structure(PvStructure {
            struct_id: NT_SCALAR.to_string(),
            fields: vec![
                ("value".to_string(), PvField::Scalar(ScalarValue::Int(5))),
                ("alarm".to_string(), PvField::Scalar(ScalarValue::Int(9))),
            ],
        });
        let mut cur = before.clone();
        let err = apply_marked_delta(&desc, &bits(&[4]), 0, &flat, &mut cur).unwrap_err();
        assert!(
            matches!(err, ValueDescMismatch::VariantMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(cur, before);
        // ...and harmless when nothing below it is marked.
        apply_marked_delta(&desc, &bits(&[1]), 0, &flat, &mut cur).unwrap();
        assert_eq!(
            cur,
            alarmed_value(ScalarValue::Int(5), ScalarValue::Int(0), "ok")
        );
    }
}
