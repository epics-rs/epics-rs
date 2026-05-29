//! High-level [`Value`] container with mark state and dotted-path access.
//!
//! Mirrors pvxs `Value` (data.h / data.cpp) for the most common
//! ergonomic operations:
//!
//! ```ignore
//! use epics_pva_rs::pvdata::{Value, ScalarType};
//! use epics_pva_rs::nt::NTScalar;
//!
//! let mut v = Value::create_from(NTScalar::new(ScalarType::Int).build());
//! v.set("value", 42i64).unwrap();
//! v.mark("value");
//! assert_eq!(v.get_as::<i64>("value").unwrap(), 42);
//! assert!(v.is_marked("value"));
//! ```
//!
//! Differences from pvxs:
//! - We don't expose `operator[]` syntax — Rust forbids overloading. Use
//!   `get`/`set`/`get_as` instead.
//! - `Value` owns its `PvField` + `BitSet`. pvxs uses shared-pointer
//!   semantics; we use `Clone` (deep copy).
//! - Type coercion supports the common cases (int↔float↔string) but not
//!   the full pvData conversion matrix.

use std::sync::Arc;

use crate::proto::BitSet;
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// High-level value container.
#[derive(Clone)]
pub struct Value {
    desc: Arc<FieldDesc>,
    /// Owned PvField tree; defaults are created from `desc` on first use.
    field: PvField,
    /// Mark state — bit positions follow pvData spec §5.4 depth-first.
    marks: BitSet,
}

/// Errors from [`Value`] operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValueError {
    #[error("no field at path '{0}'")]
    NoField(String),
    #[error("type mismatch at '{path}': expected {expected}, got {got}")]
    TypeMismatch {
        path: String,
        expected: String,
        got: String,
    },
    #[error("value at '{path}' cannot be converted to {target}")]
    NoConvert { path: String, target: String },
}

impl Value {
    /// Create a default-initialized value matching `desc`.
    pub fn create_from(desc: FieldDesc) -> Self {
        let field = default_for(&desc);
        Self {
            desc: Arc::new(desc),
            field,
            marks: BitSet::new(),
        }
    }

    /// Construct a `Value` from already-built parts.
    pub fn from_parts(desc: FieldDesc, field: PvField, marks: BitSet) -> Self {
        Self {
            desc: Arc::new(desc),
            field,
            marks,
        }
    }

    /// Borrow the type descriptor.
    pub fn desc(&self) -> &FieldDesc {
        &self.desc
    }

    /// Borrow the value tree.
    pub fn field(&self) -> &PvField {
        &self.field
    }

    /// Borrow the mark state.
    pub fn marks(&self) -> &BitSet {
        &self.marks
    }

    /// Take ownership of the parts (for codec integration).
    pub fn into_parts(self) -> (FieldDesc, PvField, BitSet) {
        let desc = match Arc::try_unwrap(self.desc) {
            Ok(d) => d,
            Err(arc) => (*arc).clone(),
        };
        (desc, self.field, self.marks)
    }

    /// True iff `path` exists.
    pub fn has(&self, path: &str) -> bool {
        self.desc.bit_for_path(path).is_some()
    }

    /// True iff the leaf at `path` is marked.
    pub fn is_marked(&self, path: &str) -> bool {
        match self.desc.bit_for_path(path) {
            Some(bit) => self.marks.get(bit),
            None => false,
        }
    }

    /// Mark the leaf at `path`. Returns `Err(NoField)` if missing.
    pub fn mark(&mut self, path: &str) -> Result<(), ValueError> {
        match self.desc.bit_for_path(path) {
            Some(bit) => {
                self.marks.set(bit);
                Ok(())
            }
            None => Err(ValueError::NoField(path.into())),
        }
    }

    /// Unmark the leaf at `path` (single bit).
    pub fn unmark(&mut self, path: &str) -> Result<(), ValueError> {
        match self.desc.bit_for_path(path) {
            Some(bit) => {
                self.marks.clear(bit);
                Ok(())
            }
            None => Err(ValueError::NoField(path.into())),
        }
    }

    /// Clear all marks (recursive).
    pub fn unmark_all(&mut self) {
        self.marks = BitSet::new();
    }

    /// Mark every bit in this value (root + all descendants).
    pub fn mark_all(&mut self) {
        let total = self.desc.total_bits();
        for i in 0..total {
            self.marks.set(i);
        }
    }

    /// Read the leaf at `path` as a typed value with coercion.
    pub fn get_as<T: FromScalarValue>(&self, path: &str) -> Result<T, ValueError> {
        let leaf = self.lookup_scalar(path)?;
        T::from_scalar(leaf).map_err(|()| ValueError::NoConvert {
            path: path.into(),
            target: std::any::type_name::<T>().into(),
        })
    }

    /// Set the leaf at `path` from a typed value with coercion. Marks
    /// the leaf bit. The target type is taken from the descriptor.
    pub fn set<T: IntoScalarValue>(&mut self, path: &str, v: T) -> Result<(), ValueError> {
        let bit = self
            .desc
            .bit_for_path(path)
            .ok_or_else(|| ValueError::NoField(path.into()))?;
        let target_type = self
            .scalar_type_at(path)
            .ok_or_else(|| ValueError::NoField(path.into()))?;
        let new_scalar = v
            .into_scalar(target_type)
            .map_err(|()| ValueError::NoConvert {
                path: path.into(),
                target: format!("{target_type:?}"),
            })?;
        self.write_scalar(path, new_scalar)?;
        self.marks.set(bit);
        Ok(())
    }

    /// Same-type empty clone — a fresh `Value` with this `Value`'s
    /// type descriptor, default-initialized fields, and an empty mark
    /// bitset. Mirrors pvxs `Value::cloneEmpty()` (data.cpp:132).
    /// Useful for PUT building: caller `set()`s the fields they want
    /// to update, then sends the `Value` over the wire — only marked
    /// leaves are encoded.
    pub fn clone_empty(&self) -> Self {
        Self::create_from((*self.desc).clone())
    }

    /// pvxs naming alias for [`Self::set`]. `copy_in` (and the
    /// fallible `try_copy_in`) match the pvxs Value API exactly so
    /// callers porting from pvxs code don't have to translate names.
    /// Both share the underlying coercion logic.
    pub fn copy_in<T: IntoScalarValue>(&mut self, path: &str, v: T) -> Result<(), ValueError> {
        self.set(path, v)
    }

    /// pvxs alias — see [`Self::copy_in`]. Returns false on failure
    /// instead of `Err`, matching pvxs's `tryCopyIn` semantics.
    pub fn try_copy_in<T: IntoScalarValue>(&mut self, path: &str, v: T) -> bool {
        self.set(path, v).is_ok()
    }

    /// pvxs naming alias for [`Self::get_as`].
    pub fn copy_out<T: FromScalarValue>(&self, path: &str) -> Result<T, ValueError> {
        self.get_as(path)
    }

    /// pvxs alias — see [`Self::copy_out`]. Returns `None` on failure
    /// instead of `Err`, matching pvxs's `tryCopyOut` semantics.
    pub fn try_copy_out<T: FromScalarValue>(&self, path: &str) -> Option<T> {
        self.get_as(path).ok()
    }

    /// Assign from another `Value`, copying the **marked** source fields
    /// into the destination.
    ///
    /// pvxs `Value::assign()` delegates to `copyIn(&o, StoreType::Compound)`
    /// whose struct branch iterates `src.imarked()` only (data.cpp:731-769):
    /// an unmarked source field is not part of the update and leaves the
    /// destination unchanged, and a marked source field with no matching
    /// destination field throws `NoField` (data.cpp:746-756) rather than
    /// being silently dropped. Driving the copy from every descriptor leaf
    /// instead copied stale unmarked values and hid schema mismatches.
    pub fn assign(&mut self, other: &Value) -> Result<(), ValueError> {
        for path in other.iter_marked() {
            // Marked source field absent in the destination: pvxs throws
            // NoField; we do too instead of skipping it.
            let Some(dst_desc) = self.desc_at(&path).cloned() else {
                return Err(ValueError::NoField(path));
            };
            let Some(src_field) = other.lookup_field(&path) else {
                continue;
            };
            // Dispatch the payload copy on the destination leaf's store
            // type, mirroring pvxs `copyIn()` which switches on
            // `StoreType` (data.cpp:609-769): scalars coerce, scalar
            // arrays copy with element conversion, and Struct/Union/Any
            // (and their array forms) copy through.
            match &dst_desc {
                FieldDesc::Scalar(t) => {
                    if let PvField::Scalar(sv) = src_field {
                        if let Some(coerced) = coerce_scalar(sv, *t) {
                            let _ = self.write_scalar(&path, coerced);
                        }
                    }
                }
                FieldDesc::ScalarArray(t) => {
                    let elems: Vec<ScalarValue> = match src_field {
                        PvField::ScalarArray(v) => v.clone(),
                        PvField::ScalarArrayTyped(a) => a.to_scalar_values(),
                        _ => continue,
                    };
                    // pvxs converts element types and faults when a value
                    // does not fit; we surface that as an error rather
                    // than returning Ok after dropping the array.
                    let converted = elems
                        .iter()
                        .map(|e| coerce_scalar(e, *t))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| ValueError::NoConvert {
                            path: path.clone(),
                            target: format!("{t:?}[]"),
                        })?;
                    let _ = self.write_field(&path, PvField::ScalarArray(converted));
                }
                // Compound / "any" leaves: same-shape copy of the source
                // store. BoundedString is a string scalar on the value
                // side, so a clone is the faithful copy here too.
                FieldDesc::StructureArray { .. }
                | FieldDesc::Union { .. }
                | FieldDesc::UnionArray { .. }
                | FieldDesc::Variant
                | FieldDesc::VariantArray
                | FieldDesc::BoundedString(_) => {
                    let _ = self.write_field(&path, src_field.clone());
                }
                // A marked structure node represents its whole subtree
                // (pvxs imarked, testdata.cpp:180-209). Copy the subtree
                // payload so a bare structure-level mark transfers; any
                // individually-marked descendant leaf is also visited and
                // re-copied (with scalar coercion) later in this loop.
                FieldDesc::Structure { .. } => {
                    if matches!(src_field, PvField::Structure(_)) {
                        let _ = self.write_field(&path, src_field.clone());
                    }
                }
            }
            // Every path here came from the source mark set, so the
            // destination field is now part of the update.
            let _ = self.mark(&path);
        }
        Ok(())
    }

    /// Iterate every field path — structure nodes AND leaves — in
    /// pvData bit order, excluding the root.
    ///
    /// Mirrors pvxs `Value::iall()` (testdata.cpp:159-178): an
    /// `NTScalar` yields 9 descendants, including the `alarm` and
    /// `timeStamp` structure nodes, not only scalar leaves. Use
    /// [`Self::iter_all_leaves`] for leaf-only iteration.
    pub fn iter_all(&self) -> impl Iterator<Item = String> + '_ {
        walk_all_paths(&self.desc, "").into_iter()
    }

    /// Iterate only leaf field paths (no structure nodes), depth-first.
    pub fn iter_all_leaves(&self) -> impl Iterator<Item = String> + '_ {
        walk_leaf_paths(&self.desc, "").into_iter()
    }

    /// Iterate immediate-children paths (depth 1).
    pub fn iter_children(&self) -> Vec<String> {
        match &*self.desc {
            FieldDesc::Structure { fields, .. } => fields.iter().map(|(n, _)| n.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Iterate every currently-marked field path, structure nodes
    /// included, in bit order.
    ///
    /// pvxs `imarked()` reports a marked structure node alongside its
    /// leaves (testdata.cpp:180-209): marking `alarm` makes `imarked()`
    /// see the `alarm` node plus its three leaves. A bare structure-node
    /// mark is therefore visible here and flows through
    /// [`Self::assign`], where the previous leaf-only walk dropped it.
    pub fn iter_marked(&self) -> Vec<String> {
        walk_all_paths(&self.desc, "")
            .into_iter()
            .filter(|p| self.is_marked(p))
            .collect()
    }

    // ── internal helpers ─────────────────────────────────────────────

    /// Resolve a dotted path to the leaf [`FieldDesc`], descending only
    /// through structure nodes. Shared traversal for the scalar and
    /// compound copy paths.
    fn desc_at(&self, path: &str) -> Option<&FieldDesc> {
        let mut cur: &FieldDesc = &self.desc;
        for seg in path.split('.').filter(|s| !s.is_empty()) {
            match cur {
                FieldDesc::Structure { fields, .. } => {
                    cur = &fields.iter().find(|(n, _)| n == seg)?.1;
                }
                _ => return None,
            }
        }
        Some(cur)
    }

    fn scalar_type_at(&self, path: &str) -> Option<ScalarType> {
        match self.desc_at(path)? {
            FieldDesc::Scalar(t) => Some(*t),
            _ => None,
        }
    }

    /// Resolve a dotted path to the leaf [`PvField`], descending only
    /// through structure nodes.
    fn lookup_field(&self, path: &str) -> Option<&PvField> {
        let mut field = &self.field;
        for seg in path.split('.').filter(|s| !s.is_empty()) {
            match field {
                PvField::Structure(s) => field = s.get_field(seg)?,
                _ => return None,
            }
        }
        Some(field)
    }

    fn lookup_scalar(&self, path: &str) -> Result<ScalarValue, ValueError> {
        match self.lookup_field(path) {
            Some(PvField::Scalar(s)) => Ok(s.clone()),
            Some(other) => Err(ValueError::TypeMismatch {
                path: path.into(),
                expected: "scalar".into(),
                got: format!("{other:?}"),
            }),
            None => Err(ValueError::NoField(path.into())),
        }
    }

    fn write_scalar(&mut self, path: &str, value: ScalarValue) -> Result<(), ValueError> {
        self.write_field(path, PvField::Scalar(value))
    }

    /// Replace the whole [`PvField`] at a dotted path, descending only
    /// through structure nodes. Used by both scalar writes and the
    /// compound-payload copy in [`Self::assign`].
    fn write_field(&mut self, path: &str, value: PvField) -> Result<(), ValueError> {
        let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        Self::write_field_in(&mut self.field, &segs, value, path)
    }

    fn write_field_in(
        field: &mut PvField,
        segs: &[&str],
        value: PvField,
        full_path: &str,
    ) -> Result<(), ValueError> {
        if segs.is_empty() {
            *field = value;
            return Ok(());
        }
        let head = segs[0];
        let tail = &segs[1..];
        match field {
            PvField::Structure(s) => {
                for (n, f) in s.fields.iter_mut() {
                    if n == head {
                        return Self::write_field_in(f, tail, value, full_path);
                    }
                }
                Err(ValueError::NoField(full_path.into()))
            }
            _ => Err(ValueError::NoField(full_path.into())),
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Value")
            .field("desc", &self.desc)
            .field("field", &self.field)
            .field("marks", &self.marks.iter().collect::<Vec<_>>())
            .finish()
    }
}

/// Build a default-initialized `PvField` matching `desc`. Mirrors the
/// helper used by `encode::default_value_for` but accessible from
/// the pvdata module surface.
fn default_for(desc: &FieldDesc) -> PvField {
    match desc {
        FieldDesc::Scalar(t) => PvField::Scalar(default_scalar(*t)),
        FieldDesc::ScalarArray(_) => PvField::ScalarArray(Vec::new()),
        FieldDesc::Variant => PvField::Variant(Box::new(crate::pvdata::VariantValue {
            desc: None,
            value: PvField::Null,
        })),
        FieldDesc::VariantArray => PvField::VariantArray(Vec::new()),
        FieldDesc::BoundedString(_) => PvField::Scalar(ScalarValue::String(String::new())),
        FieldDesc::Structure { struct_id, fields } => {
            let mut s = PvStructure::new(struct_id);
            for (n, child) in fields {
                s.fields.push((n.clone(), default_for(child)));
            }
            PvField::Structure(s)
        }
        FieldDesc::StructureArray { .. } => PvField::StructureArray(Vec::new()),
        FieldDesc::Union {
            struct_id,
            variants,
        } => {
            let _ = struct_id;
            // pvxs default: null union
            let _ = variants;
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            }
        }
        FieldDesc::UnionArray { .. } => PvField::UnionArray(Vec::new()),
    }
}

fn default_scalar(t: ScalarType) -> ScalarValue {
    match t {
        ScalarType::Boolean => ScalarValue::Boolean(false),
        ScalarType::Byte => ScalarValue::Byte(0),
        ScalarType::Short => ScalarValue::Short(0),
        ScalarType::Int => ScalarValue::Int(0),
        ScalarType::Long => ScalarValue::Long(0),
        ScalarType::UByte => ScalarValue::UByte(0),
        ScalarType::UShort => ScalarValue::UShort(0),
        ScalarType::UInt => ScalarValue::UInt(0),
        ScalarType::ULong => ScalarValue::ULong(0),
        ScalarType::Float => ScalarValue::Float(0.0),
        ScalarType::Double => ScalarValue::Double(0.0),
        ScalarType::String => ScalarValue::String(String::new()),
    }
}

/// Walk every field path — structure nodes and leaves — in pvData bit
/// order (§5.4), excluding the root. Each node's path is emitted before
/// its descendants, matching the depth-first bit numbering in
/// [`FieldDesc::bit_for_path`]: e.g. an `NTScalar` yields `value`,
/// `alarm`, `alarm.severity`, …, `timeStamp`, `timeStamp.secondsPastEpoch`, …
/// — pvxs `iall()`'s 9 descendants.
fn walk_all_paths(desc: &FieldDesc, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let FieldDesc::Structure { fields, .. } = desc {
        for (name, child) in fields {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            // The node itself occupies a bit before any of its children.
            out.push(path.clone());
            if matches!(child, FieldDesc::Structure { .. }) {
                out.extend(walk_all_paths(child, &path));
            }
        }
    }
    out
}

/// Walk leaf paths for `desc` (depth-first §5.4).
fn walk_leaf_paths(desc: &FieldDesc, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    match desc {
        FieldDesc::Structure { fields, .. } => {
            for (name, child) in fields {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                if matches!(child, FieldDesc::Structure { .. }) {
                    out.extend(walk_leaf_paths(child, &path));
                } else {
                    out.push(path);
                }
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
        }
    }
    out
}

/// Coerce one scalar value into another scalar type. Returns `None`
/// for unsupported pairs. Currently covers numeric↔numeric, numeric↔string,
/// string↔string, and bool↔bool.
fn coerce_scalar(sv: &ScalarValue, target: ScalarType) -> Option<ScalarValue> {
    use ScalarValue::*;
    let as_i64: Option<i64> = match sv {
        Boolean(b) => Some(if *b { 1 } else { 0 }),
        Byte(x) => Some(*x as i64),
        Short(x) => Some(*x as i64),
        Int(x) => Some(*x as i64),
        Long(x) => Some(*x),
        UByte(x) => Some(*x as i64),
        UShort(x) => Some(*x as i64),
        UInt(x) => Some(*x as i64),
        ULong(x) => Some(*x as i64),
        Float(x) => Some(*x as i64),
        Double(x) => Some(*x as i64),
        String(s) => s.parse::<i64>().ok(),
    };
    let as_f64: Option<f64> = match sv {
        Float(x) => Some(*x as f64),
        Double(x) => Some(*x),
        String(s) => s.parse::<f64>().ok(),
        _ => as_i64.map(|i| i as f64),
    };
    let as_string: std::string::String = match sv {
        Boolean(b) => b.to_string(),
        Byte(x) => x.to_string(),
        Short(x) => x.to_string(),
        Int(x) => x.to_string(),
        Long(x) => x.to_string(),
        UByte(x) => x.to_string(),
        UShort(x) => x.to_string(),
        UInt(x) => x.to_string(),
        ULong(x) => x.to_string(),
        Float(x) => x.to_string(),
        Double(x) => x.to_string(),
        String(s) => s.clone(),
    };
    Some(match target {
        ScalarType::Boolean => Boolean(as_i64? != 0),
        ScalarType::Byte => Byte(as_i64? as i8),
        ScalarType::Short => Short(as_i64? as i16),
        ScalarType::Int => Int(as_i64? as i32),
        ScalarType::Long => Long(as_i64?),
        ScalarType::UByte => UByte(as_i64? as u8),
        ScalarType::UShort => UShort(as_i64? as u16),
        ScalarType::UInt => UInt(as_i64? as u32),
        ScalarType::ULong => ULong(as_i64? as u64),
        ScalarType::Float => Float(as_f64? as f32),
        ScalarType::Double => Double(as_f64?),
        ScalarType::String => String(as_string),
    })
}

// ── from/into conversion traits ──────────────────────────────────────────

/// Trait for typed reads via [`Value::get_as`].
#[allow(clippy::result_unit_err)]
pub trait FromScalarValue: Sized {
    fn from_scalar(sv: ScalarValue) -> Result<Self, ()>;
}

/// Trait for typed writes via [`Value::set`]. Implementors convert
/// themselves into the descriptor's target scalar type.
#[allow(clippy::result_unit_err)]
pub trait IntoScalarValue {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()>;
}

macro_rules! impl_from_scalar_int {
    ($($t:ty),*) => {
        $(impl FromScalarValue for $t {
            fn from_scalar(sv: ScalarValue) -> Result<Self, ()> {
                let target = match std::any::type_name::<$t>() {
                    "i8" => ScalarType::Byte,
                    "i16" => ScalarType::Short,
                    "i32" => ScalarType::Int,
                    "i64" => ScalarType::Long,
                    "u8" => ScalarType::UByte,
                    "u16" => ScalarType::UShort,
                    "u32" => ScalarType::UInt,
                    "u64" => ScalarType::ULong,
                    _ => return Err(()),
                };
                let coerced = coerce_scalar(&sv, target).ok_or(())?;
                match coerced {
                    ScalarValue::Byte(x) => Ok(x as $t),
                    ScalarValue::Short(x) => Ok(x as $t),
                    ScalarValue::Int(x) => Ok(x as $t),
                    ScalarValue::Long(x) => Ok(x as $t),
                    ScalarValue::UByte(x) => Ok(x as $t),
                    ScalarValue::UShort(x) => Ok(x as $t),
                    ScalarValue::UInt(x) => Ok(x as $t),
                    ScalarValue::ULong(x) => Ok(x as $t),
                    _ => Err(()),
                }
            }
        })*
    };
}

impl_from_scalar_int!(i8, i16, i32, i64, u8, u16, u32, u64);

impl FromScalarValue for f32 {
    fn from_scalar(sv: ScalarValue) -> Result<Self, ()> {
        let coerced = coerce_scalar(&sv, ScalarType::Float).ok_or(())?;
        if let ScalarValue::Float(f) = coerced {
            Ok(f)
        } else {
            Err(())
        }
    }
}

impl FromScalarValue for f64 {
    fn from_scalar(sv: ScalarValue) -> Result<Self, ()> {
        let coerced = coerce_scalar(&sv, ScalarType::Double).ok_or(())?;
        if let ScalarValue::Double(f) = coerced {
            Ok(f)
        } else {
            Err(())
        }
    }
}

impl FromScalarValue for bool {
    fn from_scalar(sv: ScalarValue) -> Result<Self, ()> {
        let coerced = coerce_scalar(&sv, ScalarType::Boolean).ok_or(())?;
        if let ScalarValue::Boolean(b) = coerced {
            Ok(b)
        } else {
            Err(())
        }
    }
}

impl FromScalarValue for String {
    fn from_scalar(sv: ScalarValue) -> Result<Self, ()> {
        let coerced = coerce_scalar(&sv, ScalarType::String).ok_or(())?;
        if let ScalarValue::String(s) = coerced {
            Ok(s)
        } else {
            Err(())
        }
    }
}

macro_rules! impl_into_scalar_int {
    ($($t:ty),*) => {
        $(impl IntoScalarValue for $t {
            fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
                coerce_scalar(&ScalarValue::Long(self as i64), target).ok_or(())
            }
        })*
    };
}

impl_into_scalar_int!(i8, i16, i32, i64, u8, u16, u32, u64);

impl IntoScalarValue for f32 {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::Float(self), target).ok_or(())
    }
}

impl IntoScalarValue for f64 {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::Double(self), target).ok_or(())
    }
}

impl IntoScalarValue for bool {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::Boolean(self), target).ok_or(())
    }
}

impl IntoScalarValue for &str {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::String(self.to_string()), target).ok_or(())
    }
}

impl IntoScalarValue for String {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::String(self), target).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nt::NTScalar;

    fn make_nt_int() -> Value {
        Value::create_from(NTScalar::new(ScalarType::Int).build())
    }

    #[test]
    fn create_from_default_inits_value() {
        let v = make_nt_int();
        assert_eq!(v.get_as::<i64>("value").unwrap(), 0);
        assert!(!v.is_marked("value"));
    }

    #[test]
    fn set_marks_and_stores() {
        let mut v = make_nt_int();
        v.set("value", 42i32).unwrap();
        assert!(v.is_marked("value"));
        assert_eq!(v.get_as::<i32>("value").unwrap(), 42);
    }

    #[test]
    fn set_with_type_coercion_int_to_string() {
        let mut v = Value::create_from(NTScalar::new(ScalarType::String).build());
        v.set("value", 42i64).unwrap();
        assert_eq!(v.get_as::<String>("value").unwrap(), "42");
    }

    #[test]
    fn set_with_type_coercion_string_to_int() {
        let mut v = make_nt_int();
        v.set("value", "123").unwrap();
        assert_eq!(v.get_as::<i64>("value").unwrap(), 123);
    }

    #[test]
    fn unmark_clears_individual_bit() {
        let mut v = make_nt_int();
        v.mark("value").unwrap();
        v.mark("alarm.severity").unwrap();
        assert!(v.is_marked("value"));
        v.unmark("value").unwrap();
        assert!(!v.is_marked("value"));
        assert!(v.is_marked("alarm.severity"));
    }

    #[test]
    fn unmark_all_clears_everything() {
        let mut v = make_nt_int();
        v.mark("value").unwrap();
        v.mark("alarm.severity").unwrap();
        v.unmark_all();
        assert!(!v.is_marked("value"));
        assert!(!v.is_marked("alarm.severity"));
    }

    #[test]
    fn iter_children_returns_top_level_field_names() {
        let v = make_nt_int();
        let children = v.iter_children();
        assert_eq!(children, vec!["value", "alarm", "timeStamp"]);
    }

    #[test]
    fn iter_all_includes_structure_nodes_in_bit_order() {
        // pvxs iall() = 9 descendants for NTScalar (the `alarm` and
        // `timeStamp` structure nodes plus 7 scalar leaves), in bit order.
        let v = make_nt_int();
        let all: Vec<String> = v.iter_all().collect();
        assert_eq!(
            all,
            vec![
                "value",
                "alarm",
                "alarm.severity",
                "alarm.status",
                "alarm.message",
                "timeStamp",
                "timeStamp.secondsPastEpoch",
                "timeStamp.nanoseconds",
                "timeStamp.userTag",
            ]
        );
    }

    #[test]
    fn iter_all_leaves_excludes_structure_nodes() {
        let v = make_nt_int();
        let leaves: Vec<String> = v.iter_all_leaves().collect();
        assert_eq!(leaves.len(), 7);
        assert!(!leaves.contains(&"alarm".to_string()));
    }

    #[test]
    fn iter_marked_only_lists_marked_fields() {
        let mut v = make_nt_int();
        v.mark("value").unwrap();
        v.mark("alarm.message").unwrap();
        let marked = v.iter_marked();
        assert_eq!(marked, vec!["value", "alarm.message"]);
    }

    #[test]
    fn iter_marked_reports_marked_structure_node() {
        // pvxs: marking `alarm` makes imarked() see the alarm node; the
        // leaves stay unmarked unless marked individually.
        let mut v = make_nt_int();
        v.mark("alarm").unwrap();
        assert_eq!(v.iter_marked(), vec!["alarm"]);
        assert!(v.is_marked("alarm"));
        assert!(!v.is_marked("alarm.severity"));
    }

    #[test]
    fn assign_copies_marked_structure_subtree() {
        // A bare structure-level mark transfers the whole subtree.
        let mut src = make_nt_int();
        src.set("alarm.severity", 3i32).unwrap();
        src.set("alarm.status", 1i32).unwrap();
        src.unmark("alarm.severity").unwrap();
        src.unmark("alarm.status").unwrap();
        src.mark("alarm").unwrap(); // only the structure node is marked
        let mut dst = make_nt_int();
        dst.assign(&src).unwrap();
        assert_eq!(dst.get_as::<i32>("alarm.severity").unwrap(), 3);
        assert_eq!(dst.get_as::<i32>("alarm.status").unwrap(), 1);
        assert!(dst.is_marked("alarm"));
    }

    #[test]
    fn set_unknown_path_errors() {
        let mut v = make_nt_int();
        let err = v.set("nonexistent.path", 1i32).unwrap_err();
        assert!(matches!(err, ValueError::NoField(_)));
    }

    #[test]
    fn assign_copies_marked_fields() {
        let mut a = make_nt_int();
        let mut b = make_nt_int();
        b.set("value", 17i32).unwrap();
        b.mark("alarm.severity").unwrap();
        a.assign(&b).unwrap();
        assert_eq!(a.get_as::<i32>("value").unwrap(), 17);
        assert!(a.is_marked("value"));
        assert!(a.is_marked("alarm.severity"));
    }

    // ── assign() is driven by the source mark set — PVA-RS-2026-05-28-103 ──

    #[test]
    fn assign_does_not_copy_unmarked_source_scalar() {
        // Source carries value=5 but it is NOT marked: pvxs leaves the
        // destination unchanged for unmarked source fields.
        let mut src = make_nt_int();
        src.set("value", 5i32).unwrap();
        src.unmark("value").unwrap();
        let mut dst = make_nt_int();
        dst.assign(&src).unwrap();
        assert_eq!(dst.get_as::<i32>("value").unwrap(), 0);
        assert!(!dst.is_marked("value"));
    }

    #[test]
    fn assign_marked_field_absent_in_dest_errors() {
        // Source marks `b`, which the destination descriptor lacks: pvxs
        // throws NoField rather than silently skipping it.
        let src_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("a".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let mut src = Value::create_from(src_desc);
        src.set("b", 1i32).unwrap();
        let mut dst = Value::create_from(dst_desc);
        let err = dst.assign(&src).unwrap_err();
        assert!(matches!(err, ValueError::NoField(ref p) if p == "b"));
    }

    // ── assign() copies non-scalar payloads — PVA-RS-2026-05-28-102 ──
    //
    // pvxs copyIn() (data.cpp:609-769) copies array / union / any store
    // types; the old scalar-only loop dropped them while still copying
    // the mark, leaving the destination empty but advertising a change.

    /// Build a single-field structure value carrying `field` at `name`,
    /// with that field marked.
    fn one_field_value(name: &str, fdesc: FieldDesc, field: PvField) -> Value {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(name.into(), fdesc)],
        };
        let mut s = PvStructure::new("");
        s.fields.push((name.into(), field));
        let mut marks = BitSet::new();
        marks.set(1); // root=0, first child=1
        Value::from_parts(desc, PvField::Structure(s), marks)
    }

    #[test]
    fn assign_copies_scalar_array_payload() {
        let src = one_field_value(
            "value",
            FieldDesc::ScalarArray(ScalarType::Double),
            PvField::ScalarArray(vec![ScalarValue::Double(1.5), ScalarValue::Double(2.5)]),
        );
        let mut dst = Value::create_from((*src.desc).clone());
        dst.assign(&src).unwrap();
        match dst.lookup_field("value").unwrap() {
            PvField::ScalarArray(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[1], ScalarValue::Double(2.5));
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }
        assert!(dst.is_marked("value"));
    }

    #[test]
    fn assign_scalar_array_converts_element_type() {
        // Source element type Int, destination element type Long: pvxs
        // converts; we must not drop the payload.
        let src = one_field_value(
            "value",
            FieldDesc::ScalarArray(ScalarType::Int),
            PvField::ScalarArray(vec![ScalarValue::Int(7), ScalarValue::Int(8)]),
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Long))],
        };
        let mut dst = Value::create_from(dst_desc);
        dst.assign(&src).unwrap();
        match dst.lookup_field("value").unwrap() {
            PvField::ScalarArray(v) => {
                assert_eq!(v[0], ScalarValue::Long(7));
                assert_eq!(v[1], ScalarValue::Long(8));
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }
    }

    #[test]
    fn assign_copies_selected_union_payload() {
        let union_desc = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("s".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        };
        let src = one_field_value(
            "u",
            union_desc.clone(),
            PvField::Union {
                selector: 0,
                variant_name: "i".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Int(42))),
            },
        );
        let mut dst = Value::create_from((*src.desc).clone());
        dst.assign(&src).unwrap();
        match dst.lookup_field("u").unwrap() {
            PvField::Union {
                selector, value, ..
            } => {
                assert_eq!(*selector, 0);
                assert_eq!(**value, PvField::Scalar(ScalarValue::Int(42)));
            }
            other => panic!("expected Union, got {other:?}"),
        }
        assert!(dst.is_marked("u"));
    }

    #[test]
    fn assign_copies_variant_payload() {
        let src = one_field_value(
            "any",
            FieldDesc::Variant,
            PvField::Variant(Box::new(crate::pvdata::VariantValue {
                desc: Some(FieldDesc::Scalar(ScalarType::Int)),
                value: PvField::Scalar(ScalarValue::Int(99)),
            })),
        );
        let mut dst = Value::create_from((*src.desc).clone());
        dst.assign(&src).unwrap();
        match dst.lookup_field("any").unwrap() {
            PvField::Variant(v) => {
                assert_eq!(v.value, PvField::Scalar(ScalarValue::Int(99)));
            }
            other => panic!("expected Variant, got {other:?}"),
        }
        assert!(dst.is_marked("any"));
    }

    #[test]
    fn assign_copies_structure_array_payload() {
        let elem_fields = vec![("a".into(), FieldDesc::Scalar(ScalarType::Int))];
        let mut elem = PvStructure::new("");
        elem.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Int(5))));
        let src = one_field_value(
            "rows",
            FieldDesc::StructureArray {
                struct_id: String::new(),
                fields: elem_fields,
            },
            PvField::StructureArray(vec![Some(elem)]),
        );
        let mut dst = Value::create_from((*src.desc).clone());
        dst.assign(&src).unwrap();
        match dst.lookup_field("rows").unwrap() {
            PvField::StructureArray(rows) => {
                assert_eq!(rows.len(), 1);
                let row = rows[0].as_ref().expect("present element");
                assert_eq!(
                    row.get_field("a"),
                    Some(&PvField::Scalar(ScalarValue::Int(5)))
                );
            }
            other => panic!("expected StructureArray, got {other:?}"),
        }
        assert!(dst.is_marked("rows"));
    }

    #[test]
    fn assign_scalar_array_conversion_failure_errors() {
        // A non-numeric string element cannot convert to Int: pvxs faults;
        // we return NoConvert rather than Ok after dropping the array.
        let src = one_field_value(
            "value",
            FieldDesc::ScalarArray(ScalarType::String),
            PvField::ScalarArray(vec![ScalarValue::String("not a number".into())]),
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Int))],
        };
        let mut dst = Value::create_from(dst_desc);
        let err = dst.assign(&src).unwrap_err();
        assert!(matches!(err, ValueError::NoConvert { .. }));
    }
}
