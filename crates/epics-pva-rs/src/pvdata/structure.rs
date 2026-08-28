//! Composite (`PvStructure`) and field (`PvField`) value types.

use std::fmt;

use super::field::FieldDesc;
use super::scalar::ScalarValue;
use super::typed_array::TypedScalarArray;

/// Runtime PV field value (recursive). Mirrors the full pvData value space.
#[derive(Debug, Clone, PartialEq)]
pub enum PvField {
    Scalar(ScalarValue),
    /// Generic scalar array — backwards-compatible enum-tagged form.
    /// Cloning is O(n) (deep copy). For new code prefer
    /// [`Self::ScalarArrayTyped`] which carries an `Arc<[T]>` and
    /// clones in O(1). Encoders accept either variant; the typed
    /// path takes a single bulk `memcpy` when host endian matches
    /// wire endian.
    ScalarArray(Vec<ScalarValue>),
    /// Typed scalar array — refcount-shared, zero-copy-friendly.
    /// pvxs `shared_array<T>` analogue. Constructors:
    /// [`PvField::scalar_array_double`], `_float`, `_int`, etc.
    ScalarArrayTyped(TypedScalarArray),
    Structure(PvStructure),
    /// Structure array. each element is `Option` — `None` is a
    /// pvxs `0x00` null element (absent), `Some(s)` a `0x01`-present
    /// element. This is distinct from a *present* element whose body is
    /// empty; overloading an inner sentinel could not express that.
    StructureArray(Vec<Option<PvStructure>>),
    /// A union value — `selector >= 0` and `value` is the chosen variant's
    /// concrete `PvField`. `selector == -1` indicates a null union (no
    /// variant selected).
    Union {
        selector: i32,
        variant_name: String,
        value: Box<PvField>,
    },
    /// Union array. `None` is a `0x00` null element (absent); `Some(item)`
    /// is a present element selecting a real variant (`selector >= 0`).
    /// A `Some(item)` whose selector is `-1`/out-of-range selects no
    /// variant and is *not* a distinct wire state: pvxs's UnionA decoder
    /// collapses a present null selector to absent
    /// (`dataencode.cpp:635-637`), so the encoder canonicalizes such an
    /// element to the absent (`0x00`) form and a round trip yields `None`.
    UnionArray(Vec<Option<UnionItem>>),
    /// "Any" — variant carries its own [`FieldDesc`]. Empty descriptor +
    /// null value indicates "no value".
    Variant(Box<VariantValue>),
    /// Variant ("any") array. `None` is a `0x00` null element (absent);
    /// `Some(v)` is a present element carrying a descriptor + value.
    /// A `Some(v)` with no descriptor is *not* a distinct wire state: pvxs's
    /// AnyA decoder collapses a present null descriptor to absent
    /// (`dataencode.cpp:669-675`), so the encoder canonicalizes such an
    /// element to the absent (`0x00`) form and a round trip yields `None`.
    VariantArray(Vec<Option<VariantValue>>),
    /// Explicit empty value (used by null union / null variant).
    Null,
}

/// One element of a union array — same shape as the [`PvField::Union`] arm.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionItem {
    pub selector: i32,
    pub variant_name: String,
    pub value: PvField,
}

/// Variant value: a [`FieldDesc`] paired with its concrete value. An empty
/// variant (no value present) carries the `null` field discriminator.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantValue {
    pub desc: Option<FieldDesc>,
    pub value: PvField,
}

impl VariantValue {
    /// An `any` variant carrying a scalar value, tagged with the scalar's
    /// own descriptor. Mirrors pvxs assigning a scalar `Value` into an
    /// `Any` member — the common case for filling an advertised `any`
    /// slot (e.g. an NTAttribute `value`) without hand-building the
    /// descriptor/value pair.
    pub fn scalar(v: ScalarValue) -> Self {
        Self {
            desc: Some(FieldDesc::Scalar(v.scalar_type())),
            value: PvField::Scalar(v),
        }
    }

    /// An empty (`null`) variant — the `any` slot is present but carries
    /// no value, the field-discriminator `null` case on the wire.
    pub fn null() -> Self {
        Self {
            desc: None,
            value: PvField::Null,
        }
    }
}

impl fmt::Display for PvField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scalar(v) => write!(f, "{v}"),
            Self::ScalarArray(arr) => {
                write!(f, "[")?;
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Self::ScalarArrayTyped(arr) => write!(f, "{arr}"),
            Self::Structure(s) => write!(f, "{s}"),
            Self::StructureArray(arr) => {
                write!(f, "[")?;
                for (i, s) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match s {
                        Some(s) => write!(f, "{s}")?,
                        None => write!(f, "(null)")?,
                    }
                }
                write!(f, "]")
            }
            Self::Union {
                selector,
                variant_name,
                value,
            } => {
                if *selector < 0 {
                    write!(f, "(null)")
                } else {
                    write!(f, "{variant_name}={value}")
                }
            }
            Self::UnionArray(items) => {
                write!(f, "[")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match it {
                        // `None` = absent element; a present item with
                        // `selector < 0` = present null-selector union.
                        None => write!(f, "(null)")?,
                        Some(it) if it.selector < 0 => write!(f, "(null)")?,
                        Some(it) => write!(f, "{}={}", it.variant_name, it.value)?,
                    }
                }
                write!(f, "]")
            }
            Self::Variant(v) => write!(f, "{}", v.value),
            Self::VariantArray(items) => {
                write!(f, "[")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match it {
                        Some(it) => write!(f, "{}", it.value)?,
                        None => write!(f, "(null)")?,
                    }
                }
                write!(f, "]")
            }
            Self::Null => write!(f, "null"),
        }
    }
}

impl PvField {
    /// Construct a typed `Double` scalar array. Wraps `data` in
    /// `Arc<[f64]>` so subsequent clones are refcount bumps.
    pub fn scalar_array_double(data: impl Into<std::sync::Arc<[f64]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Double(data.into()))
    }
    pub fn scalar_array_float(data: impl Into<std::sync::Arc<[f32]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Float(data.into()))
    }
    pub fn scalar_array_int(data: impl Into<std::sync::Arc<[i32]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Int(data.into()))
    }
    pub fn scalar_array_long(data: impl Into<std::sync::Arc<[i64]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Long(data.into()))
    }
    pub fn scalar_array_short(data: impl Into<std::sync::Arc<[i16]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Short(data.into()))
    }
    pub fn scalar_array_byte(data: impl Into<std::sync::Arc<[i8]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::Byte(data.into()))
    }
    pub fn scalar_array_ubyte(data: impl Into<std::sync::Arc<[u8]>>) -> Self {
        Self::ScalarArrayTyped(super::TypedScalarArray::UByte(data.into()))
    }

    /// Dereference a *selected* union / non-empty variant ("any") to the
    /// concrete value it carries, mirroring pvxs `Value::lookup("->")`.
    ///
    /// pvxs follows a union/`Any` to its active member before reading the
    /// payload — e.g. `pvaGetValue` does
    /// `if(value.type()==Any || value.type()==Union) value =
    /// value.lookup("->")` before the array/scalar conversion
    /// (`pvxs/ioc/pvalink_lset.cpp:278-279`). The standard EPICS
    /// `NTNDArray` carries its image data as a discriminated `Union`
    /// `value` member (`pvxs/src/nt.cpp:208-220`), so a consumer that
    /// stops at the union sees no usable value.
    ///
    /// * [`PvField::Union`] with a selected variant (`selector >= 0`)
    ///   resolves to the chosen variant's value.
    /// * [`PvField::Variant`] carrying a descriptor resolves to its value.
    /// * A null union (`selector == -1`) or empty variant (no descriptor)
    ///   has no selected payload and resolves to itself — the caller then
    ///   sees a union/variant and treats it as "no concrete value".
    /// * Every other field resolves to itself.
    ///
    /// Idempotent on non-union/variant fields, and recursive: a union
    /// whose selected variant is itself a union/variant is followed
    /// through to the leaf concrete value.
    pub fn deref_selected(&self) -> &PvField {
        match self {
            PvField::Union {
                selector, value, ..
            } if *selector >= 0 => value.deref_selected(),
            PvField::Variant(v) if v.desc.is_some() => v.value.deref_selected(),
            other => other,
        }
    }

    /// Recover a [`FieldDesc`] from a concrete value. Used by paths
    /// that cache values without a separate descriptor (e.g. QSRV
    /// PVA-plugin PV registry, gateway snapshot replay).
    ///
    /// **Lossy paths.** Several variants cannot be fully reconstructed
    /// from the value alone and produce a best-effort or degraded
    /// descriptor. Callers that need wire-faithful introspection
    /// (e.g. archiver-engine forwarding the original channel
    /// descriptor through a gateway) must thread the original
    /// [`FieldDesc`] alongside the value rather than rely on this
    /// recovery:
    ///
    /// - `ScalarArray` with no elements falls back to `Double`.
    /// - `StructureArray` with no elements yields `struct_id=""` and
    ///   no fields.
    /// - `Union` reports only the currently-selected variant — sibling
    ///   variants in the original descriptor are not recoverable.
    /// - `UnionArray` returns an **empty** variants list. The per-item
    ///   `selector` is an index into the original variants vector, so
    ///   a best-effort reconstruction from the items would either
    ///   misalign indices (when not every slot is exercised) or omit
    ///   types entirely; both are wrong on the wire. Top-level
    ///   `UnionArray` PVs routed through this recovery are therefore
    ///   not round-trippable here. Producers that need wire-faithful
    ///   introspection should hand the canonical descriptor through
    ///   their registration API (e.g.
    ///   `epics_bridge_rs::qsrv::PvaPvHandle::descriptor`) so the
    ///   server emits it directly instead of calling this recovery.
    /// - `Variant` with no captured descriptor and bare `Null` both
    ///   degrade to `FieldDesc::Variant`.
    pub fn descriptor(&self) -> FieldDesc {
        match self {
            Self::Scalar(v) => FieldDesc::Scalar(v.scalar_type()),
            Self::ScalarArray(arr) => FieldDesc::ScalarArray(
                arr.first()
                    .map(|v| v.scalar_type())
                    .unwrap_or(super::ScalarType::Double),
            ),
            Self::ScalarArrayTyped(arr) => FieldDesc::ScalarArray(arr.scalar_type()),
            Self::Structure(s) => FieldDesc::Structure {
                struct_id: s.struct_id.clone(),
                fields: s
                    .fields
                    .iter()
                    .map(|(n, f)| (n.clone(), f.descriptor()))
                    .collect(),
            },
            Self::StructureArray(arr) => {
                // Recover the element schema from the first PRESENT
                // element; null (`None`) elements carry no schema.
                let (struct_id, fields) = arr
                    .iter()
                    .flatten()
                    .next()
                    .map(|s| {
                        (
                            s.struct_id.clone(),
                            s.fields
                                .iter()
                                .map(|(n, f)| (n.clone(), f.descriptor()))
                                .collect(),
                        )
                    })
                    .unwrap_or_default();
                FieldDesc::StructureArray { struct_id, fields }
            }
            Self::Union {
                variant_name,
                value,
                ..
            } => FieldDesc::Union {
                struct_id: String::new(),
                variants: vec![(variant_name.clone(), value.descriptor())],
            },
            // Lossy: see `descriptor()` doc. Reconstructing variants
            // from items would misalign per-item `selector` indices.
            Self::UnionArray(_) => FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: Vec::new(),
            },
            Self::Variant(v) => v.desc.clone().unwrap_or(FieldDesc::Variant),
            Self::VariantArray(_) => FieldDesc::VariantArray,
            Self::Null => FieldDesc::Variant,
        }
    }

    /// Like [`Self::descriptor`] but **wire-faithful or nothing**:
    /// returns `Some(desc)` only when the descriptor can be fully
    /// recovered from the value, and `None` for the lossy shapes
    /// [`Self::descriptor`] documents. Used by the `any`
    /// encoder so a bare value never advertises a degraded/wrong
    /// schema on the wire — see `encode.rs` `FieldDesc::Variant`.
    ///
    /// `None` cases (cannot be reconstructed from the value alone):
    /// - empty untyped `ScalarArray` (no element type),
    /// - empty `StructureArray` (no element schema),
    /// - `Union` (sibling variants unknown),
    /// - `UnionArray` (variant list / selector indices unknown),
    /// - bare `Null`,
    /// - any `Structure`/`StructureArray` that *contains* a `None`
    ///   field (the loss propagates).
    ///
    /// Faithful cases: scalars, non-empty/typed scalar arrays,
    /// fully-recoverable structures, `Variant` carrying a descriptor,
    /// and `VariantArray` (whose `FieldDesc` is complete on its own).
    pub fn wire_descriptor(&self) -> Option<FieldDesc> {
        Some(match self {
            Self::Scalar(v) => FieldDesc::Scalar(v.scalar_type()),
            // Empty untyped array has no recoverable element type.
            Self::ScalarArray(arr) => FieldDesc::ScalarArray(arr.first()?.scalar_type()),
            Self::ScalarArrayTyped(arr) => FieldDesc::ScalarArray(arr.scalar_type()),
            Self::Structure(s) => {
                let mut fields = Vec::with_capacity(s.fields.len());
                for (n, f) in &s.fields {
                    fields.push((n.clone(), f.wire_descriptor()?));
                }
                FieldDesc::Structure {
                    struct_id: s.struct_id.clone(),
                    fields,
                }
            }
            Self::StructureArray(arr) => {
                // Wire-faithful schema needs a present element to read
                // it from; an all-null (or empty) array cannot supply one.
                let first = arr.iter().flatten().next()?;
                let mut fields = Vec::with_capacity(first.fields.len());
                for (n, f) in &first.fields {
                    fields.push((n.clone(), f.wire_descriptor()?));
                }
                FieldDesc::StructureArray {
                    struct_id: first.struct_id.clone(),
                    fields,
                }
            }
            // Sibling variants / per-item selector indices are not
            // recoverable from a bare value.
            Self::Union { .. } | Self::UnionArray(_) => return None,
            // The inner descriptor travels with the value; the outer
            // shape is the complete `FieldDesc::Variant`.
            Self::Variant(v) => match v.desc {
                Some(_) => FieldDesc::Variant,
                None => return None,
            },
            Self::VariantArray(_) => FieldDesc::VariantArray,
            Self::Null => return None,
        })
    }
}

/// A PVA structure with ordered named fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PvStructure {
    pub struct_id: String,
    pub fields: Vec<(String, PvField)>,
}

impl PvStructure {
    pub fn new(struct_id: &str) -> Self {
        Self {
            struct_id: struct_id.to_string(),
            fields: Vec::new(),
        }
    }

    pub fn get_field(&self, name: &str) -> Option<&PvField> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn get_field_mut(&mut self, name: &str) -> Option<&mut PvField> {
        self.fields
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    pub fn get_value(&self) -> Option<&ScalarValue> {
        match self.get_field("value")? {
            PvField::Scalar(v) => Some(v),
            _ => None,
        }
    }

    pub fn get_alarm(&self) -> Option<&PvStructure> {
        match self.get_field("alarm")? {
            PvField::Structure(s) => Some(s),
            _ => None,
        }
    }

    pub fn get_timestamp(&self) -> Option<&PvStructure> {
        match self.get_field("timeStamp")? {
            PvField::Structure(s) => Some(s),
            _ => None,
        }
    }

    /// Add (or overwrite) a field.
    pub fn set(&mut self, name: &str, value: PvField) {
        for entry in &mut self.fields {
            if entry.0 == name {
                entry.1 = value;
                return;
            }
        }
        self.fields.push((name.to_string(), value));
    }
}

impl fmt::Display for PvStructure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // For display, just show the value field if it exists (NTScalar-like).
        if let Some(val) = self.get_value() {
            write!(f, "{val}")
        } else {
            write!(f, "structure {} {{", self.struct_id)?;
            for (name, field) in &self.fields {
                write!(f, " {name}={field}")?;
            }
            write!(f, " }}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wire_descriptor` is `Some` only for shapes whose
    /// descriptor is fully recoverable from the value, `None` for the
    /// lossy ones (so the `any` encoder won't emit a wrong schema).
    #[test]
    fn wire_descriptor_faithful_vs_lossy() {
        // Faithful.
        assert!(
            PvField::Scalar(ScalarValue::Int(1))
                .wire_descriptor()
                .is_some()
        );
        assert!(
            PvField::scalar_array_int(Vec::<i32>::new())
                .wire_descriptor()
                .is_some()
        ); // typed empty: type known
        assert!(
            PvField::ScalarArray(vec![ScalarValue::Int(1)])
                .wire_descriptor()
                .is_some()
        );
        assert!(
            PvField::VariantArray(Vec::new())
                .wire_descriptor()
                .is_some()
        );
        let s = {
            let mut s = PvStructure::new("x");
            s.fields
                .push(("v".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            PvField::Structure(s)
        };
        assert!(s.wire_descriptor().is_some());

        // Lossy → None.
        assert!(
            PvField::ScalarArray(Vec::new()).wire_descriptor().is_none(),
            "empty untyped array has no element type"
        );
        assert!(
            PvField::StructureArray(Vec::new())
                .wire_descriptor()
                .is_none(),
            "empty structure array has no element schema"
        );
        assert!(
            PvField::Union {
                selector: 0,
                variant_name: "i".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Int(1))),
            }
            .wire_descriptor()
            .is_none(),
            "union loses sibling variants"
        );
        assert!(
            PvField::UnionArray(Vec::new()).wire_descriptor().is_none(),
            "union array loses variant list / selector indices"
        );
        assert!(PvField::Null.wire_descriptor().is_none());

        // Loss propagates: a structure containing a lossy field is lossy.
        let nested = {
            let mut s = PvStructure::new("x");
            s.fields
                .push(("u".into(), PvField::ScalarArray(Vec::new())));
            PvField::Structure(s)
        };
        assert!(
            nested.wire_descriptor().is_none(),
            "lossy field makes the whole structure lossy"
        );
    }

    #[test]
    fn empty_structure() {
        let s = PvStructure::new("epics:nt/NTScalar:1.0");
        assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
        assert!(s.fields.is_empty());
        assert!(s.get_value().is_none());
    }

    #[test]
    fn lookup_value_alarm_timestamp() {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.set("value", PvField::Scalar(ScalarValue::Double(7.5)));
        s.set("alarm", PvField::Structure(PvStructure::new("alarm_t")));
        s.set("timeStamp", PvField::Structure(PvStructure::new("time_t")));
        assert_eq!(s.get_value(), Some(&ScalarValue::Double(7.5)));
        assert_eq!(s.get_alarm().unwrap().struct_id, "alarm_t");
        assert_eq!(s.get_timestamp().unwrap().struct_id, "time_t");
    }

    #[test]
    fn set_overwrites() {
        let mut s = PvStructure::new("test");
        s.set("v", PvField::Scalar(ScalarValue::Int(1)));
        s.set("v", PvField::Scalar(ScalarValue::Int(2)));
        assert_eq!(s.fields.len(), 1);
        if let Some(PvField::Scalar(ScalarValue::Int(n))) = s.get_field("v") {
            assert_eq!(*n, 2);
        } else {
            panic!("expected scalar int");
        }
    }
}
