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

use epics_base_rs::types::PvString;

use crate::proto::BitSet;
use crate::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, UnionItem, VariantValue,
};

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
        self.bit_for_value_path(path).is_some()
    }

    /// True iff the leaf at `path` is marked.
    pub fn is_marked(&self, path: &str) -> bool {
        match self.bit_for_value_path(path) {
            Some(bit) => self.marks.get(bit),
            None => false,
        }
    }

    /// Mark the leaf at `path`. Returns `Err(NoField)` if missing.
    pub fn mark(&mut self, path: &str) -> Result<(), ValueError> {
        match self.bit_for_value_path(path) {
            Some(bit) => {
                self.marks.set(bit);
                Ok(())
            }
            None => Err(ValueError::NoField(path.into())),
        }
    }

    /// Unmark the leaf at `path` (single bit).
    pub fn unmark(&mut self, path: &str) -> Result<(), ValueError> {
        match self.bit_for_value_path(path) {
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
    /// the leaf's container bit. The target scalar type is taken from the
    /// leaf's current (typed) value. The path may traverse union/any
    /// members and compound-array elements (e.g. `value->floatValue`,
    /// `array[0].field`).
    ///
    /// A `->member` segment **selects** the union arm via the mutable
    /// traversal in [`Self::lookup_mut`] (pvxs `traverse(modify=true)`):
    /// a null or differently-selected union is switched to `member` and
    /// its default payload allocated, so the freshly-allocated scalar
    /// carries the descriptor's type that this write then coerces into.
    /// The addressed leaf must still resolve to a scalar — `set()` writes
    /// scalars only and does not grow an array.
    pub fn set<T: IntoScalarValue>(&mut self, path: &str, v: T) -> Result<(), ValueError> {
        let bit = self.bit_for_value_path(path);
        let leaf = self
            .lookup_mut(path)
            .ok_or_else(|| ValueError::NoField(path.into()))?;
        let target_type = match leaf {
            PvField::Scalar(sv) => sv.scalar_type(),
            _ => return Err(ValueError::NoField(path.into())),
        };
        let new_scalar = v
            .into_scalar(target_type)
            .map_err(|()| ValueError::NoConvert {
                path: path.into(),
                target: format!("{target_type:?}"),
            })?;
        *leaf = PvField::Scalar(new_scalar);
        if let Some(bit) = bit {
            self.marks.set(bit);
        }
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
    /// destination unchanged, and a marked source *leaf* with no matching
    /// destination field throws `NoField` (data.cpp:746-756) rather than
    /// being silently dropped. Driving the copy from every descriptor leaf
    /// instead copied stale unmarked values and hid schema mismatches.
    ///
    /// A marked structure *node* propagates only its changed-bit — pvxs
    /// calls `dfld.mark()` for a marked sub-struct and copies NO subtree
    /// payload (data.cpp:739-744). The node's leaves transfer only when
    /// individually marked, so a bare `alarm` mark leaves the destination's
    /// `alarm.*` leaves untouched.
    ///
    /// A marked union (or union-array element) is reselected by *descriptor*
    /// rather than copied verbatim: pvxs matches the source's selected
    /// member descriptor against the destination union variants and assigns
    /// into that arm (data.cpp:683-703), throwing `NoConvert` when none
    /// matches. Preserving the source selector index/name instead made the
    /// descriptor-driven encoder write the payload under the wrong arm when
    /// the destination variant order differed. An "any" (Variant) field is
    /// stored verbatim, matching pvxs's Any branch (data.cpp:668-681).
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
                // pvxs `copyIn` for a `Struct[]` destination enforces the
                // destination element schema (data.cpp:617-642): any
                // element whose descriptor differs from the destination
                // member descriptor is rebuilt in the destination schema
                // and copied field-by-field (`allocMember()` +
                // `Value::assign`, data.cpp:635-636), and only when *every*
                // element already matches does the whole array store
                // verbatim (the `!convert` fast path, data.cpp:642).
                FieldDesc::StructureArray {
                    struct_id: dst_struct_id,
                    fields: dst_fields,
                } => {
                    let PvField::StructureArray(src_items) = src_field else {
                        // A non-`Struct[]` payload cannot satisfy a
                        // `Struct[]` destination; pvxs throws NoConvert
                        // rather than silently dropping it (data.cpp:660).
                        return Err(ValueError::NoConvert {
                            path: path.clone(),
                            target: "struct[]".into(),
                        });
                    };
                    let Some(FieldDesc::StructureArray {
                        struct_id: src_struct_id,
                        fields: src_fields,
                    }) = other.desc_at(&path)
                    else {
                        return Err(ValueError::NoConvert {
                            path: path.clone(),
                            target: "struct[]".into(),
                        });
                    };
                    if src_fields == dst_fields {
                        // Identical element schema → verbatim clone, the
                        // pvxs `!convert` fast path (data.cpp:642). Do not
                        // pay the per-element rebuild when squashing a
                        // same-schema source.
                        let _ = self.write_field(&path, src_field.clone());
                    } else {
                        // Cross-schema: convert every present element into
                        // the destination element schema field-by-field,
                        // mirroring pvxs `scratch[i] = allocMember();
                        // scratch[i].assign(tsrc[i])` (data.cpp:635-636).
                        let src_elem_desc = FieldDesc::Structure {
                            struct_id: src_struct_id.clone(),
                            fields: src_fields.clone(),
                        };
                        let dst_elem_desc = FieldDesc::Structure {
                            struct_id: dst_struct_id.clone(),
                            fields: dst_fields.clone(),
                        };
                        let mut out = Vec::with_capacity(src_items.len());
                        for item in src_items {
                            match item {
                                // A present `Struct[]` element is fully
                                // populated (no per-element wire bitset),
                                // so mark every source field and let
                                // `assign` project them onto the
                                // destination schema — the same struct-to-
                                // struct copyIn pvxs runs over the source's
                                // marked fields (data.cpp:735-757), which
                                // already coerces scalars, reselects union
                                // arms, and faults `NoField` on a source
                                // field absent from the destination.
                                Some(src_struct) => {
                                    let mut src_elem = Value::from_parts(
                                        src_elem_desc.clone(),
                                        PvField::Structure(src_struct.clone()),
                                        BitSet::new(),
                                    );
                                    src_elem.mark_all();
                                    let mut dst_elem = Value::create_from(dst_elem_desc.clone());
                                    dst_elem.assign(&src_elem)?;
                                    let (_, field, _) = dst_elem.into_parts();
                                    let PvField::Structure(s) = field else {
                                        unreachable!("Structure desc yields a Structure field");
                                    };
                                    out.push(Some(s));
                                }
                                // A null element stays null: pvxs converts
                                // only `if(tsrc[i])` (data.cpp:634).
                                None => out.push(None),
                            }
                        }
                        let _ = self.write_field(&path, PvField::StructureArray(out));
                    }
                }
                // "Any" (Variant) and its array form hold arbitrary
                // self-describing values: pvxs `copyIn` stores the source
                // verbatim with no descriptor reselection (Any at
                // data.cpp:669-681; `AnyA` skips the member-type
                // enforcement — the convert block runs only when
                // `desc->code != TypeCode::AnyA`, data.cpp:623).
                FieldDesc::Variant | FieldDesc::VariantArray => {
                    let _ = self.write_field(&path, src_field.clone());
                }
                // A union destination reselects its arm by *descriptor*,
                // not by the source selector index or member name: pvxs
                // `copyIn` matches the source's selected member descriptor
                // against the destination variants and assigns into that
                // arm, throwing `NoConvert` when none matches
                // (data.cpp:683-703). Copying the source union verbatim
                // kept the source selector even when the destination
                // variant order differed, so the descriptor-driven encoder
                // wrote the payload under the wrong destination arm.
                FieldDesc::Union {
                    variants: dst_variants,
                    ..
                } => {
                    let Some(FieldDesc::Union {
                        variants: src_variants,
                        ..
                    }) = other.desc_at(&path)
                    else {
                        return Err(ValueError::NoConvert {
                            path: path.clone(),
                            target: "union".into(),
                        });
                    };
                    let converted = reselect_union(src_field, src_variants, dst_variants, &path)?;
                    let _ = self.write_field(&path, converted);
                }
                // UnionArray reselects each present element's arm the same
                // way; an unselected (`selector < 0`) present element
                // canonicalizes to absent, matching the UnionA encoder.
                FieldDesc::UnionArray {
                    variants: dst_variants,
                    ..
                } => {
                    let Some(FieldDesc::UnionArray {
                        variants: src_variants,
                        ..
                    }) = other.desc_at(&path)
                    else {
                        return Err(ValueError::NoConvert {
                            path: path.clone(),
                            target: "union[]".into(),
                        });
                    };
                    let PvField::UnionArray(src_items) = src_field else {
                        return Err(ValueError::NoConvert {
                            path: path.clone(),
                            target: "union[]".into(),
                        });
                    };
                    let mut out = Vec::with_capacity(src_items.len());
                    for item in src_items {
                        out.push(match item {
                            Some(it) => reselect_union_item(it, src_variants, dst_variants, &path)?,
                            None => None,
                        });
                    }
                    let _ = self.write_field(&path, PvField::UnionArray(out));
                }
                // A marked structure node only propagates the changed-bit
                // — pvxs `copyIn` calls `dfld.mark()` for a marked
                // sub-struct and copies NO subtree payload (data.cpp:
                // 739-744). The node's leaves transfer only when they are
                // individually marked and visited as their own paths in
                // this loop. Copying the source subtree here overwrote
                // destination leaves that a bare structure-node mark must
                // leave untouched, and installed unvalidated source
                // children on a schema mismatch. The `self.mark(&path)`
                // below records the changed-bit; the payload stays put.
                FieldDesc::Structure { .. } => {}
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
    /// Mirrors pvxs `Value::iall()` (testdata.cpp:167-171): an
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
    /// leaves (testdata.cpp:180-198): marking `alarm` makes `imarked()`
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

    /// Resolve a dotted path to the leaf [`PvField`], descending only
    /// through structure nodes. Used by [`Self::assign`], whose paths
    /// always come from the descriptor's structure walk.
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
        match self.lookup(path) {
            Some(PvField::Scalar(s)) => Ok(s.clone()),
            Some(other) => Err(ValueError::TypeMismatch {
                path: path.into(),
                expected: "scalar".into(),
                got: format!("{other:?}"),
            }),
            None => Err(ValueError::NoField(path.into())),
        }
    }

    /// Resolve a pvxs-style path to a borrowed leaf [`PvField`].
    ///
    /// Supports `.` (structure member), `->` / `->member` (union/`any`
    /// dereference and selection), and `[index]` (compound-array
    /// element) — mirroring pvxs `Value::traverse` (data.cpp:783-950).
    /// Returns `None` when a segment is missing, a `->member` names a
    /// variant other than the one currently selected, a union/any is
    /// null, or an array index is out of range / a null element. The
    /// parent operator `<` is not supported: a root `Value` exposes no
    /// parent handle to walk up to.
    pub fn lookup(&self, path: &str) -> Option<&PvField> {
        let segs = parse_path(path).ok()?;
        let mut cur = Cur::Field(&self.field);
        for seg in &segs {
            cur = step(cur, seg)?;
        }
        match cur {
            Cur::Field(f) => Some(f),
            _ => None,
        }
    }

    /// Mutable counterpart of [`Self::lookup`].
    ///
    /// Unlike the read-only [`Self::lookup`], this is a *mutable*
    /// traversal — pvxs `traverse(name, modify=true, …)` (data.cpp:854-906).
    /// A `->member` segment on a union **selects** that member: when the
    /// union is null or set to a different arm, the member's default
    /// payload is allocated and the selector/variant_name are updated
    /// before descending into it (data.cpp:877-887). The same path may
    /// switch an already-selected arm. Read-only [`Self::lookup`] stays
    /// constrained to the currently-selected arm (data.cpp:888-894 errors
    /// on a const select).
    ///
    /// The descriptor is threaded alongside the value tree so the
    /// selected arm's type is known; selection inside an `any` region
    /// (where the static descriptor is lost) is limited to an
    /// already-selected matching member.
    pub fn lookup_mut(&mut self, path: &str) -> Option<&mut PvField> {
        let segs = parse_path(path).ok()?;
        // Clone the Arc so the descriptor cursor is independent of the
        // `&mut self.field` borrow taken below — both are needed at once
        // to select a union arm by its descriptor.
        let desc = Arc::clone(&self.desc);
        let mut cur = CurMut::Field(&mut self.field);
        let mut dcur: Option<&FieldDesc> = Some(desc.as_ref());
        for seg in &segs {
            let (next_cur, next_desc) = step_mut(cur, dcur, seg)?;
            cur = next_cur;
            dcur = next_desc;
        }
        match cur {
            CurMut::Field(f) => Some(f),
            _ => None,
        }
    }

    /// The mark bit a value path maps to. pvData §5.4 gives a union or
    /// compound array a single bit; its contents are not separately
    /// addressed. A path that descends through `->` or `[index]`
    /// therefore marks its nearest containing structure field. For a
    /// plain dotted path this is just the leaf's own bit.
    fn bit_for_value_path(&self, path: &str) -> Option<usize> {
        let cut = [path.find("->"), path.find('[')]
            .into_iter()
            .flatten()
            .min();
        let prefix = match cut {
            Some(i) => path[..i].trim_end_matches('.'),
            None => path,
        };
        self.desc.bit_for_path(prefix)
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

/// Match the source union's selected arm against the destination
/// variants by descriptor, returning the destination `(selector,
/// variant_name, payload)`. pvxs `copyIn` selects the destination union
/// member whose descriptor equals the source's selected member descriptor
/// (`data.cpp:692` `_equal(src.desc, &desc->members[idx])`), not by index
/// or name. Because the descriptors are equal, the source payload already
/// conforms to the destination arm, so cloning it reproduces pvxs's
/// `temp.assign(src)` for equal descriptors. Returns `None` when no
/// destination variant matches.
fn match_union_arm(
    sel: usize,
    value: &PvField,
    src_variants: &[(String, FieldDesc)],
    dst_variants: &[(String, FieldDesc)],
) -> Option<(i32, String, PvField)> {
    let src_arm_desc = &src_variants.get(sel)?.1;
    dst_variants
        .iter()
        .enumerate()
        .find(|(_, (_, ddesc))| ddesc == src_arm_desc)
        .map(|(i, (dname, _))| (i as i32, dname.clone(), value.clone()))
}

/// Reselect a `PvField::Union` into the destination union's arm by
/// descriptor (see [`match_union_arm`]). A null/unselected source union
/// clears the destination to a null union: pvxs throws `NoConvert` for an
/// unselected source, but the same-schema squash that drives [`Value::assign`]
/// can mark a null union, so clearing it keeps the copy from failing
/// wholesale. A selected source with no matching destination variant is
/// `NoConvert`.
fn reselect_union(
    src: &PvField,
    src_variants: &[(String, FieldDesc)],
    dst_variants: &[(String, FieldDesc)],
    path: &str,
) -> Result<PvField, ValueError> {
    let no_convert = || ValueError::NoConvert {
        path: path.into(),
        target: "union".into(),
    };
    match src {
        PvField::Union {
            selector, value, ..
        } if *selector >= 0 => {
            let (selector, variant_name, value) =
                match_union_arm(*selector as usize, value, src_variants, dst_variants)
                    .ok_or_else(no_convert)?;
            Ok(PvField::Union {
                selector,
                variant_name,
                value: Box::new(value),
            })
        }
        PvField::Union { .. } => Ok(PvField::Union {
            selector: -1,
            variant_name: String::new(),
            value: Box::new(PvField::Null),
        }),
        _ => Err(no_convert()),
    }
}

/// Reselect one present [`UnionItem`] into the destination union-array's
/// arm by descriptor. A present element whose selector is `< 0` selects no
/// variant and canonicalizes to absent (`None`), matching the UnionA
/// encoder; a selected element with no matching destination variant is
/// `NoConvert`.
fn reselect_union_item(
    it: &UnionItem,
    src_variants: &[(String, FieldDesc)],
    dst_variants: &[(String, FieldDesc)],
    path: &str,
) -> Result<Option<UnionItem>, ValueError> {
    if it.selector < 0 {
        return Ok(None);
    }
    let (selector, variant_name, value) =
        match_union_arm(it.selector as usize, &it.value, src_variants, dst_variants).ok_or_else(
            || ValueError::NoConvert {
                path: path.into(),
                target: "union[]".into(),
            },
        )?;
    Ok(Some(UnionItem {
        selector,
        variant_name,
        value,
    }))
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
        ScalarType::String => ScalarValue::String(PvString::new()),
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

/// One parsed segment of a high-level value path.
enum PathSeg {
    /// `.name` or a leading bare `name` — a structure member.
    Field(String),
    /// `[index]` — a compound-array element.
    Index(usize),
    /// `->` (bare) or `->member` — dereference / select a union or `any`.
    Deref(Option<String>),
}

/// Parse a pvxs-style value path into segments: `.`, `->`, `->member`,
/// and `[index]`. `<` (parent) is intentionally unsupported — a root
/// `Value` has no parent handle. A malformed path yields `NoField`.
fn parse_path(path: &str) -> Result<Vec<PathSeg>, ValueError> {
    let nofield = || ValueError::NoField(path.into());
    let mut segs = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("->") {
            let (name, r2) = take_ident(r);
            segs.push(PathSeg::Deref((!name.is_empty()).then(|| name.to_string())));
            rest = r2;
        } else if let Some(r) = rest.strip_prefix('.') {
            let (name, r2) = take_ident(r);
            if name.is_empty() {
                return Err(nofield());
            }
            segs.push(PathSeg::Field(name.to_string()));
            rest = r2;
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']').ok_or_else(nofield)?;
            let idx: usize = r[..end].parse().map_err(|_| nofield())?;
            segs.push(PathSeg::Index(idx));
            rest = &r[end + 1..];
        } else {
            let (name, r2) = take_ident(rest);
            if name.is_empty() {
                return Err(nofield());
            }
            segs.push(PathSeg::Field(name.to_string()));
            rest = r2;
        }
    }
    Ok(segs)
}

/// Read an identifier up to the next path delimiter (`.`, `[`, or the
/// `-` that begins `->`).
fn take_ident(s: &str) -> (&str, &str) {
    let end = s.find(['.', '[', '-']).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// True iff a union with `selector`/`variant_name` can be dereferenced
/// by `member`: the union must have a variant selected, and when a
/// member name is given it must be the selected one.
fn deref_union(selector: i32, variant_name: &str, member: &Option<String>) -> bool {
    if selector < 0 {
        return false;
    }
    match member {
        None => true,
        Some(m) => m == variant_name,
    }
}

/// Read-side traversal cursor — the value tree's array elements are not
/// themselves `PvField`, so the cursor tracks the concrete container.
enum Cur<'a> {
    Field(&'a PvField),
    Struct(&'a PvStructure),
    Union(&'a UnionItem),
    Variant(&'a VariantValue),
}

fn step<'a>(cur: Cur<'a>, seg: &PathSeg) -> Option<Cur<'a>> {
    match (seg, cur) {
        (PathSeg::Field(n), Cur::Field(PvField::Structure(s))) => Some(Cur::Field(s.get_field(n)?)),
        (PathSeg::Field(n), Cur::Struct(s)) => Some(Cur::Field(s.get_field(n)?)),
        (PathSeg::Index(i), Cur::Field(PvField::StructureArray(items))) => {
            Some(Cur::Struct(items.get(*i)?.as_ref()?))
        }
        (PathSeg::Index(i), Cur::Field(PvField::UnionArray(items))) => {
            Some(Cur::Union(items.get(*i)?.as_ref()?))
        }
        (PathSeg::Index(i), Cur::Field(PvField::VariantArray(items))) => {
            Some(Cur::Variant(items.get(*i)?.as_ref()?))
        }
        (
            PathSeg::Deref(m),
            Cur::Field(PvField::Union {
                selector,
                variant_name,
                value,
            }),
        ) if deref_union(*selector, variant_name, m) => Some(Cur::Field(value)),
        (PathSeg::Deref(_), Cur::Field(PvField::Variant(v))) => Some(Cur::Field(&v.value)),
        (PathSeg::Deref(m), Cur::Union(it)) if deref_union(it.selector, &it.variant_name, m) => {
            Some(Cur::Field(&it.value))
        }
        (PathSeg::Deref(_), Cur::Variant(v)) => Some(Cur::Field(&v.value)),
        _ => None,
    }
}

/// Mutable counterpart of [`Cur`].
enum CurMut<'a> {
    Field(&'a mut PvField),
    Struct(&'a mut PvStructure),
    Union(&'a mut UnionItem),
    Variant(&'a mut VariantValue),
}

/// One mutable traversal step, threading the type descriptor alongside
/// the value cursor so a union `->member` segment can **select** the
/// member by descriptor (pvxs `traverse(modify=true)`, data.cpp:854-906).
/// `dcur` is `None` once traversal enters an `any` region whose content
/// descriptor is dynamic; union selection there is limited to an
/// already-selected member.
fn step_mut<'f, 'd>(
    cur: CurMut<'f>,
    dcur: Option<&'d FieldDesc>,
    seg: &PathSeg,
) -> Option<(CurMut<'f>, Option<&'d FieldDesc>)> {
    match (seg, cur) {
        (PathSeg::Field(n), CurMut::Field(PvField::Structure(s))) => {
            let child = s.get_field_mut(n)?;
            Some((CurMut::Field(child), struct_child_desc(dcur, n)))
        }
        (PathSeg::Field(n), CurMut::Struct(s)) => {
            let child = s.get_field_mut(n)?;
            Some((CurMut::Field(child), struct_child_desc(dcur, n)))
        }
        (PathSeg::Index(i), CurMut::Field(PvField::StructureArray(items))) => {
            // The element's fields live in the StructureArray descriptor,
            // which `struct_child_desc` reads the same as a Structure.
            Some((CurMut::Struct(items.get_mut(*i)?.as_mut()?), dcur))
        }
        (PathSeg::Index(i), CurMut::Field(PvField::UnionArray(items))) => {
            // The element's variants live in the UnionArray descriptor.
            Some((CurMut::Union(items.get_mut(*i)?.as_mut()?), dcur))
        }
        (PathSeg::Index(i), CurMut::Field(PvField::VariantArray(items))) => {
            Some((CurMut::Variant(items.get_mut(*i)?.as_mut()?), None))
        }
        (
            PathSeg::Deref(m),
            CurMut::Field(PvField::Union {
                selector,
                variant_name,
                value,
            }),
        ) => select_union_member(selector, variant_name, value, dcur, m),
        (PathSeg::Deref(_), CurMut::Field(PvField::Variant(v))) => {
            Some((CurMut::Field(&mut v.value), None))
        }
        (PathSeg::Deref(m), CurMut::Union(it)) => select_union_member(
            &mut it.selector,
            &mut it.variant_name,
            &mut it.value,
            dcur,
            m,
        ),
        (PathSeg::Deref(_), CurMut::Variant(v)) => Some((CurMut::Field(&mut v.value), None)),
        _ => None,
    }
}

/// Descend the descriptor cursor into a structure member named `name`.
/// Reads both `Structure` and `StructureArray` (whose element fields are
/// the array's `fields`). Returns `None` when the descriptor is absent or
/// the member is missing.
fn struct_child_desc<'d>(dcur: Option<&'d FieldDesc>, name: &str) -> Option<&'d FieldDesc> {
    match dcur {
        Some(FieldDesc::Structure { fields, .. })
        | Some(FieldDesc::StructureArray { fields, .. }) => {
            fields.iter().find(|(n, _)| n == name).map(|(_, d)| d)
        }
        _ => None,
    }
}

/// The descriptor of a union variant named `name`, from a `Union` /
/// `UnionArray` descriptor.
fn union_variant_desc<'d>(dcur: Option<&'d FieldDesc>, name: &str) -> Option<&'d FieldDesc> {
    match dcur {
        Some(FieldDesc::Union { variants, .. }) | Some(FieldDesc::UnionArray { variants, .. }) => {
            variants.iter().find(|(n, _)| n == name).map(|(_, d)| d)
        }
        _ => None,
    }
}

/// Mutable union dereference / selection — pvxs `traverse(modify=true)`
/// union branch (data.cpp:867-905). Shared by the plain-union field arm
/// and the union-array-element arm.
///
/// - bare `->` (no member): dereference the current selection; a null
///   union (no arm selected) has nothing to dereference → `None`
///   (data.cpp:900-904).
/// - `->member` with a descriptor: look the member up in the union
///   descriptor; when it is not the currently-selected arm, allocate its
///   default payload and update the selector (select/switch,
///   data.cpp:877-887); then descend into it.
/// - `->member` without a descriptor (inside an `any` region): may only
///   dereference an already-selected matching member.
fn select_union_member<'f, 'd>(
    selector: &mut i32,
    variant_name: &mut String,
    value: &'f mut PvField,
    dcur: Option<&'d FieldDesc>,
    member: &Option<String>,
) -> Option<(CurMut<'f>, Option<&'d FieldDesc>)> {
    match member {
        None => {
            if *selector < 0 {
                return None;
            }
            let mdesc = union_variant_desc(dcur, variant_name);
            Some((CurMut::Field(value), mdesc))
        }
        Some(m) => match dcur {
            Some(FieldDesc::Union { variants, .. })
            | Some(FieldDesc::UnionArray { variants, .. }) => {
                let idx = variants.iter().position(|(n, _)| n == m)?;
                let mdesc = &variants[idx].1;
                if *selector != idx as i32 {
                    *value = default_for(mdesc);
                    *selector = idx as i32;
                    *variant_name = m.clone();
                }
                Some((CurMut::Field(value), Some(mdesc)))
            }
            _ => {
                if *selector >= 0 && variant_name == m {
                    Some((CurMut::Field(value), None))
                } else {
                    None
                }
            }
        },
    }
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
        // C++ cast semantics for the double → integer step, as pvxs's
        // `Value::copyIn` performs it (`data.cpp:551-578`): out of range
        // overflows rather than saturating. See `pvdata::cpp_cast`.
        Float(x) => Some(crate::pvdata::cpp_cast::double_to_i64(*x as f64)),
        Double(x) => Some(crate::pvdata::cpp_cast::double_to_i64(*x)),
        String(s) => s.as_str_lossy().parse::<i64>().ok(),
    };
    // pvxs dispatches on the STORE's signedness (`data.cpp:551` vs `:565`):
    // an unsigned leaf casts the double to `uint64_t`, not to `int64_t`. The
    // two agree once truncated to a narrower width — the low bits are the
    // same — but differ at full width, where `uint64_t(2^64)` is 0 while
    // `int64_t(2^64)` is `INT64_MIN` (2^63 reinterpreted).
    let as_u64: Option<u64> = match sv {
        Float(x) => Some(crate::pvdata::cpp_cast::double_to_u64(*x as f64)),
        Double(x) => Some(crate::pvdata::cpp_cast::double_to_u64(*x)),
        _ => as_i64.map(|i| i as u64),
    };
    let as_f64: Option<f64> = match sv {
        Float(x) => Some(*x as f64),
        Double(x) => Some(*x),
        String(s) => s.as_str_lossy().parse::<f64>().ok(),
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
        String(s) => s.as_str_lossy().into_owned(),
    };
    Some(match target {
        ScalarType::Boolean => Boolean(as_i64? != 0),
        ScalarType::Byte => Byte(as_i64? as i8),
        ScalarType::Short => Short(as_i64? as i16),
        ScalarType::Int => Int(as_i64? as i32),
        ScalarType::Long => Long(as_i64?),
        ScalarType::UByte => UByte(as_u64? as u8),
        ScalarType::UShort => UShort(as_u64? as u16),
        ScalarType::UInt => UInt(as_u64? as u32),
        ScalarType::ULong => ULong(as_u64?),
        ScalarType::Float => Float(as_f64? as f32),
        ScalarType::Double => Double(as_f64?),
        ScalarType::String => String(as_string.into()),
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
            Ok(s.as_str_lossy().into_owned())
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
        coerce_scalar(&ScalarValue::String(self.into()), target).ok_or(())
    }
}

impl IntoScalarValue for String {
    fn into_scalar(self, target: ScalarType) -> Result<ScalarValue, ()> {
        coerce_scalar(&ScalarValue::String(self.into()), target).ok_or(())
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
    fn assign_marked_structure_node_marks_only_no_subtree_copy() {
        // pvxs copyIn's struct branch (data.cpp:739-744) calls dfld.mark()
        // for a marked sub-struct and copies NO subtree payload. A bare
        // `alarm` mark therefore propagates only the changed-bit; the
        // destination's own alarm leaves stay untouched.
        let mut src = make_nt_int();
        src.set("alarm.severity", 3i32).unwrap();
        src.set("alarm.status", 1i32).unwrap();
        src.unmark("alarm.severity").unwrap();
        src.unmark("alarm.status").unwrap();
        src.mark("alarm").unwrap(); // only the structure node is marked
        let mut dst = make_nt_int();
        dst.set("alarm.severity", 7i32).unwrap();
        dst.set("alarm.status", 9i32).unwrap();
        dst.assign(&src).unwrap();
        // Destination leaves preserved — no subtree payload was copied.
        assert_eq!(dst.get_as::<i32>("alarm.severity").unwrap(), 7);
        assert_eq!(dst.get_as::<i32>("alarm.status").unwrap(), 9);
        // The structure-node changed-bit still propagates.
        assert!(dst.is_marked("alarm"));
    }

    #[test]
    fn assign_parent_plus_leaf_mark_copies_only_marked_leaf() {
        // Marking the `alarm` node AND the `alarm.severity` leaf transfers
        // only the individually-marked leaf; the unmarked sibling
        // (`alarm.status`) keeps the destination's own value.
        let mut src = make_nt_int();
        src.set("alarm.severity", 3i32).unwrap();
        src.set("alarm.status", 1i32).unwrap();
        src.unmark("alarm.status").unwrap();
        src.mark("alarm").unwrap(); // node + the still-marked severity leaf
        let mut dst = make_nt_int();
        dst.set("alarm.severity", 7i32).unwrap();
        dst.set("alarm.status", 9i32).unwrap();
        dst.assign(&src).unwrap();
        assert_eq!(dst.get_as::<i32>("alarm.severity").unwrap(), 3);
        assert_eq!(dst.get_as::<i32>("alarm.status").unwrap(), 9);
        assert!(dst.is_marked("alarm"));
        assert!(dst.is_marked("alarm.severity"));
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

    // ── path traversal of union/any/array ──

    fn struct_of(name: &str, fdesc: FieldDesc, field: PvField) -> Value {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(name.into(), fdesc)],
        };
        let mut s = PvStructure::new("");
        s.fields.push((name.into(), field));
        Value::from_parts(desc, PvField::Structure(s), BitSet::new())
    }

    fn union_value_desc() -> FieldDesc {
        FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("floatValue".into(), FieldDesc::Scalar(ScalarType::Float)),
                ("intValue".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        }
    }

    #[test]
    fn lookup_union_selected_member() {
        let v = struct_of(
            "value",
            union_value_desc(),
            PvField::Union {
                selector: 0,
                variant_name: "floatValue".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Float(1.5))),
            },
        );
        assert_eq!(v.get_as::<f32>("value->floatValue").unwrap(), 1.5);
        // A non-selected member is not reachable.
        assert!(v.lookup("value->intValue").is_none());
        // Bare `->` dereferences the current selection.
        assert!(matches!(
            v.lookup("value->"),
            Some(PvField::Scalar(ScalarValue::Float(_)))
        ));
    }

    #[test]
    fn set_union_selected_member_and_marks_container() {
        let mut v = struct_of(
            "value",
            union_value_desc(),
            PvField::Union {
                selector: 0,
                variant_name: "floatValue".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Float(0.0))),
            },
        );
        v.set("value->floatValue", 2.5f32).unwrap();
        assert_eq!(v.get_as::<f32>("value->floatValue").unwrap(), 2.5);
        // The union occupies one bit; marking its member marks `value`.
        assert!(v.is_marked("value"));
        assert!(v.is_marked("value->floatValue"));
    }

    #[test]
    fn set_selects_null_union_member() {
        // A freshly-created value has a null union (selector -1); pvxs
        // mutable traversal (data.cpp:877-887) selects the named member
        // when writing, so `set("value->floatValue", …)` must succeed and
        // allocate the Float arm. Previously the value-only traversal
        // returned NoField on a null union.
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), union_value_desc())],
        };
        let mut v = Value::create_from(desc);
        v.set("value->floatValue", 2.5f32).unwrap();
        assert_eq!(v.get_as::<f32>("value->floatValue").unwrap(), 2.5);
        assert!(v.is_marked("value"));
        // Bare `->` now dereferences the freshly-selected Float arm.
        assert!(matches!(
            v.lookup("value->"),
            Some(PvField::Scalar(ScalarValue::Float(_)))
        ));
    }

    #[test]
    fn set_switches_selected_union_member() {
        // Writing a *different* member switches the arm (pvxs reselects
        // when fld.desc != the requested member, data.cpp:879-883).
        let mut v = struct_of(
            "value",
            union_value_desc(),
            PvField::Union {
                selector: 0,
                variant_name: "floatValue".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Float(1.0))),
            },
        );
        v.set("value->intValue", 7i32).unwrap();
        assert_eq!(v.get_as::<i32>("value->intValue").unwrap(), 7);
        // The Float arm is no longer selected, so the const read can't
        // reach it.
        assert!(v.lookup("value->floatValue").is_none());
        assert!(v.is_marked("value"));
    }

    #[test]
    fn lookup_mut_selects_null_union_member_allocating_default() {
        // The public mutable lookup selects too: on a null union it
        // allocates the member's default payload (here Float(0.0)).
        let mut v = struct_of(
            "value",
            union_value_desc(),
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            },
        );
        let leaf = v.lookup_mut("value->floatValue").unwrap();
        assert!(matches!(leaf, PvField::Scalar(ScalarValue::Float(f)) if *f == 0.0));
    }

    #[test]
    fn set_switches_union_array_element_member() {
        // Member selection also applies to an *existing* union-array
        // element (allocating a new array element is the separately
        // deferred half of this finding). Switch a present `d` element to
        // the `i` arm and write it.
        let mut v = struct_of(
            "cells",
            FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: vec![
                    ("d".into(), FieldDesc::Scalar(ScalarType::Double)),
                    ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                ],
            },
            PvField::UnionArray(vec![Some(UnionItem {
                selector: 0,
                variant_name: "d".into(),
                value: PvField::Scalar(ScalarValue::Double(3.5)),
            })]),
        );
        v.set("cells[0]->i", 9i32).unwrap();
        assert_eq!(v.get_as::<i32>("cells[0]->i").unwrap(), 9);
        assert!(v.lookup("cells[0]->d").is_none());
    }

    #[test]
    fn lookup_any_deref() {
        let v = struct_of(
            "any",
            FieldDesc::Variant,
            PvField::Variant(Box::new(crate::pvdata::VariantValue {
                desc: Some(FieldDesc::Scalar(ScalarType::Int)),
                value: PvField::Scalar(ScalarValue::Int(7)),
            })),
        );
        assert_eq!(v.get_as::<i32>("any->").unwrap(), 7);
    }

    #[test]
    fn lookup_structure_array_element_field() {
        let mut elem = PvStructure::new("");
        elem.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Int(5))));
        let v = struct_of(
            "rows",
            FieldDesc::StructureArray {
                struct_id: String::new(),
                fields: vec![("a".into(), FieldDesc::Scalar(ScalarType::Int))],
            },
            PvField::StructureArray(vec![Some(elem)]),
        );
        assert_eq!(v.get_as::<i32>("rows[0].a").unwrap(), 5);
        // Out-of-range index is not reachable.
        assert!(v.lookup("rows[5].a").is_none());
    }

    #[test]
    fn lookup_union_array_element_member() {
        let v = struct_of(
            "cells",
            FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: vec![("d".into(), FieldDesc::Scalar(ScalarType::Double))],
            },
            PvField::UnionArray(vec![Some(UnionItem {
                selector: 0,
                variant_name: "d".into(),
                value: PvField::Scalar(ScalarValue::Double(3.5)),
            })]),
        );
        assert_eq!(v.get_as::<f64>("cells[0]->d").unwrap(), 3.5);
    }

    // ── assign() is driven by the source mark set ──

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

    // ── assign() copies non-scalar payloads ──
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
    fn assign_union_reselects_destination_arm_by_descriptor() {
        // Source union has a single `s` (String) variant selected at
        // index 0; destination is `{i: Int, s: String}` where `s` is at
        // index 1. pvxs reselects by descriptor, so the destination must
        // end up selecting arm 1 (`s`), not the source's literal selector
        // 0 (which on the destination is the `i` Int arm).
        let src = one_field_value(
            "u",
            FieldDesc::Union {
                struct_id: String::new(),
                variants: vec![("s".into(), FieldDesc::Scalar(ScalarType::String))],
            },
            PvField::Union {
                selector: 0,
                variant_name: "s".into(),
                value: Box::new(PvField::Scalar(ScalarValue::String("hi".into()))),
            },
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(
                "u".into(),
                FieldDesc::Union {
                    struct_id: String::new(),
                    variants: vec![
                        ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("s".into(), FieldDesc::Scalar(ScalarType::String)),
                    ],
                },
            )],
        };
        let mut dst = Value::create_from(dst_desc);
        dst.assign(&src).unwrap();
        match dst.lookup_field("u").unwrap() {
            PvField::Union {
                selector,
                variant_name,
                value,
            } => {
                assert_eq!(*selector, 1, "must reselect the `s` arm, not keep src 0");
                assert_eq!(variant_name, "s");
                assert_eq!(**value, PvField::Scalar(ScalarValue::String("hi".into())));
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn assign_union_no_matching_variant_errors() {
        // Source selects a String arm the destination union does not
        // offer — pvxs throws NoConvert.
        let src = one_field_value(
            "u",
            FieldDesc::Union {
                struct_id: String::new(),
                variants: vec![("s".into(), FieldDesc::Scalar(ScalarType::String))],
            },
            PvField::Union {
                selector: 0,
                variant_name: "s".into(),
                value: Box::new(PvField::Scalar(ScalarValue::String("x".into()))),
            },
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(
                "u".into(),
                FieldDesc::Union {
                    struct_id: String::new(),
                    variants: vec![("i".into(), FieldDesc::Scalar(ScalarType::Int))],
                },
            )],
        };
        let mut dst = Value::create_from(dst_desc);
        let err = dst.assign(&src).unwrap_err();
        assert!(matches!(err, ValueError::NoConvert { .. }));
    }

    #[test]
    fn assign_union_array_reselects_each_element() {
        // Source union-array element selects `s` at index 0 in a
        // single-variant `{s}`; destination is `{i, s}` so the element
        // must reselect to dest index 1.
        let src = one_field_value(
            "ua",
            FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: vec![("s".into(), FieldDesc::Scalar(ScalarType::String))],
            },
            PvField::UnionArray(vec![
                Some(UnionItem {
                    selector: 0,
                    variant_name: "s".into(),
                    value: PvField::Scalar(ScalarValue::String("a".into())),
                }),
                None,
            ]),
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(
                "ua".into(),
                FieldDesc::UnionArray {
                    struct_id: String::new(),
                    variants: vec![
                        ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("s".into(), FieldDesc::Scalar(ScalarType::String)),
                    ],
                },
            )],
        };
        let mut dst = Value::create_from(dst_desc);
        dst.assign(&src).unwrap();
        match dst.lookup_field("ua").unwrap() {
            PvField::UnionArray(items) => {
                assert_eq!(items.len(), 2);
                let it = items[0].as_ref().expect("present element");
                assert_eq!(it.selector, 1);
                assert_eq!(it.variant_name, "s");
                assert_eq!(it.value, PvField::Scalar(ScalarValue::String("a".into())));
                assert!(items[1].is_none());
            }
            other => panic!("expected UnionArray, got {other:?}"),
        }
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
    fn assign_structure_array_cross_schema_copies_field_by_field() {
        // Source element schema differs from the destination's (`a` is a
        // Long here, an Int in the destination), so pvxs takes the
        // `convert` path: each present element is rebuilt in the
        // destination schema and copied field-by-field with coercion
        // (data.cpp:623-642). A verbatim clone would store `a` as a Long
        // under an Int descriptor; this asserts the per-element rebuild
        // instead, and that a null element survives.
        let mut elem = PvStructure::new("");
        elem.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Long(5))));
        elem.fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Double(2.5))));
        let src = one_field_value(
            "rows",
            FieldDesc::StructureArray {
                struct_id: String::new(),
                fields: vec![
                    ("a".into(), FieldDesc::Scalar(ScalarType::Long)),
                    ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
                ],
            },
            PvField::StructureArray(vec![Some(elem), None]),
        );
        let dst_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(
                "rows".into(),
                FieldDesc::StructureArray {
                    struct_id: String::new(),
                    fields: vec![
                        ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
                    ],
                },
            )],
        };
        let mut dst = Value::create_from(dst_desc);
        dst.assign(&src).unwrap();
        match dst.lookup_field("rows").unwrap() {
            PvField::StructureArray(rows) => {
                assert_eq!(rows.len(), 2);
                let row = rows[0].as_ref().expect("present element");
                // `a` is now an Int (destination schema), not the source's
                // Long — proof the element was rebuilt, not cloned verbatim.
                assert_eq!(
                    row.get_field("a"),
                    Some(&PvField::Scalar(ScalarValue::Int(5)))
                );
                assert_eq!(
                    row.get_field("b"),
                    Some(&PvField::Scalar(ScalarValue::Double(2.5)))
                );
                assert!(rows[1].is_none(), "null element must stay null");
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
