//! Type-safe Normative Types runtime.
//!
//! [`TypedNT`] is the bridge between user-defined Rust structs and
//! the wire-level [`PvField`] / [`FieldDesc`] representation. End
//! users typically don't implement this trait by hand — the
//! `#[derive(NTScalar)]` proc-macro from `epics-macros-rs` generates
//! it from a struct definition like:
//!
//! ```ignore
//! #[derive(NTScalar)]
//! struct MotorPos {
//!     value: f64,
//!     #[nt(meta)] alarm: Alarm,
//!     #[nt(meta)] timestamp: TimeStamp,
//! }
//! ```
//!
//! ## Why this exists
//!
//! Without it, every `pvget` consumer has to walk the `PvField` tree
//! and pattern-match every leaf. With it, a `pvget_typed::<MotorPos>`
//! returns the struct directly — the wire ↔ struct mapping is fixed
//! at compile time, so a missing field or type mismatch surfaces as
//! a Rust type error or a [`TypedNTError`] at the boundary.
//!
//! Mirrors the role pvxs's `Value::as<T>()` plays in C++, but with
//! Rust's stricter type system enforcing field presence and shape
//! at the trait-bound level.
//!
//! ## Manual implementation
//!
//! Implementing this trait manually is supported when the derive
//! doesn't cover an exotic shape. Provide `descriptor()`,
//! `to_pv_field(&self)`, and `from_pv_field(&PvField)`. The default
//! [`Alarm`] / [`TimeStamp`] meta types are re-exported here for
//! convenience — `#[nt(meta)] alarm: Alarm` is the canonical NT
//! shape.

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarValue};

/// Errors surfaced at the typed/untyped boundary.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TypedNTError {
    /// `from_pv_field` got a wrapper that didn't match the expected
    /// struct id (e.g. expecting `epics:nt/NTScalar:1.0` and seeing
    /// `epics:nt/NTTable:1.0`).
    #[error("wrong NT struct id: expected {expected:?}, got {got:?}")]
    WrongStructId { expected: String, got: String },
    /// A field declared in the descriptor was missing on the wire.
    #[error("missing field '{0}'")]
    MissingField(String),
    /// A field's wire shape didn't match the Rust type (e.g. expected
    /// `f64`, got `String`).
    #[error("wrong type for field '{field}': {detail}")]
    WrongType { field: String, detail: String },
}

/// A Rust type with a declared NT shape. Implemented automatically
/// by `#[derive(NTScalar)]` and friends.
pub trait TypedNT: Sized + Send + 'static {
    /// Wire-level descriptor (returned to clients on INIT, consulted
    /// on encode). Must be deterministic — every call returns an
    /// identical [`FieldDesc`] so type-cache references resolve
    /// across calls.
    fn descriptor() -> FieldDesc;

    /// Encode this value into a wire [`PvField`].
    fn to_pv_field(&self) -> PvField;

    /// Decode a wire [`PvField`] into the Rust type. Returns
    /// [`TypedNTError`] on mismatch — caller propagates as
    /// [`crate::error::PvaError::InvalidValue`] or similar.
    fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError>;
}

/// Standard `alarm_t` meta sub-structure used by every NT shape.
/// Carry this in your `#[derive(NTScalar)]` struct via
/// `#[nt(meta)] alarm: Alarm`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alarm {
    pub severity: i32,
    pub status: i32,
    pub message: String,
}

impl TypedNT for Alarm {
    fn descriptor() -> FieldDesc {
        Self::alarm_descriptor()
    }
    fn to_pv_field(&self) -> PvField {
        self.alarm_to_pv_field()
    }
    fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        Self::alarm_from_pv_field(field)
    }
}

impl Alarm {
    /// Wire descriptor — same as [`crate::nt::meta::alarm_desc`].
    /// Inherent name; the `TypedNT::descriptor()` impl forwards to
    /// this so users can call it without bringing the trait into
    /// scope.
    pub fn alarm_descriptor() -> FieldDesc {
        crate::nt::meta::alarm_desc()
    }

    pub fn alarm_to_pv_field(&self) -> PvField {
        let mut s = PvStructure::new("alarm_t");
        s.fields.push((
            "severity".into(),
            PvField::Scalar(ScalarValue::Int(self.severity)),
        ));
        s.fields.push((
            "status".into(),
            PvField::Scalar(ScalarValue::Int(self.status)),
        ));
        s.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String(self.message.clone().into())),
        ));
        PvField::Structure(s)
    }

    pub fn alarm_from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        let s = match field {
            PvField::Structure(s) => s,
            _ => {
                return Err(TypedNTError::WrongType {
                    field: "alarm".into(),
                    detail: "expected structure".into(),
                });
            }
        };
        Ok(Self {
            severity: get_i32(s, "severity").unwrap_or(0),
            status: get_i32(s, "status").unwrap_or(0),
            message: get_str(s, "message").unwrap_or_default(),
        })
    }
}

/// Standard `time_t` meta sub-structure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeStamp {
    pub seconds_past_epoch: i64,
    pub nanoseconds: i32,
    pub user_tag: i32,
}

impl TypedNT for TimeStamp {
    fn descriptor() -> FieldDesc {
        Self::ts_descriptor()
    }
    fn to_pv_field(&self) -> PvField {
        self.ts_to_pv_field()
    }
    fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        Self::ts_from_pv_field(field)
    }
}

impl TimeStamp {
    pub fn ts_descriptor() -> FieldDesc {
        crate::nt::meta::time_desc()
    }

    pub fn ts_to_pv_field(&self) -> PvField {
        let mut s = PvStructure::new("time_t");
        s.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(self.seconds_past_epoch)),
        ));
        s.fields.push((
            "nanoseconds".into(),
            PvField::Scalar(ScalarValue::Int(self.nanoseconds)),
        ));
        s.fields.push((
            "userTag".into(),
            PvField::Scalar(ScalarValue::Int(self.user_tag)),
        ));
        PvField::Structure(s)
    }

    pub fn ts_from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        let s = match field {
            PvField::Structure(s) => s,
            _ => {
                return Err(TypedNTError::WrongType {
                    field: "timestamp".into(),
                    detail: "expected structure".into(),
                });
            }
        };
        Ok(Self {
            seconds_past_epoch: get_i64(s, "secondsPastEpoch").unwrap_or(0),
            nanoseconds: get_i32(s, "nanoseconds").unwrap_or(0),
            user_tag: get_i32(s, "userTag").unwrap_or(0),
        })
    }
}

// ── Field accessors used by both Alarm/TimeStamp and the generated
//    derive code. Public-but-not-re-exported so derive expansion can
//    reach them via `epics_pva_rs::nt::typed::__rt::*`. -----------

/// Internal helpers consumed only by the `#[derive(NTScalar)]`
/// expansion. Stable surface, but operators of derive-generated
/// code don't import from here directly.
pub mod __rt {
    pub use crate::nt::typed::{Alarm, TimeStamp, TypedNT, TypedNTError};
    pub use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

    pub fn get_i32(s: &PvStructure, name: &str) -> Option<i32> {
        super::get_i32(s, name)
    }
    pub fn get_i64(s: &PvStructure, name: &str) -> Option<i64> {
        super::get_i64(s, name)
    }
    pub fn get_f32(s: &PvStructure, name: &str) -> Option<f32> {
        super::get_f32(s, name)
    }
    pub fn get_f64(s: &PvStructure, name: &str) -> Option<f64> {
        super::get_f64(s, name)
    }
    pub fn get_bool(s: &PvStructure, name: &str) -> Option<bool> {
        super::get_bool(s, name)
    }
    pub fn get_string(s: &PvStructure, name: &str) -> Option<String> {
        super::get_str(s, name)
    }

    pub fn missing(name: &str) -> TypedNTError {
        TypedNTError::MissingField(name.into())
    }

    pub fn wrong_type(field: &str, detail: &str) -> TypedNTError {
        TypedNTError::WrongType {
            field: field.into(),
            detail: detail.into(),
        }
    }

    pub fn wrong_struct_id(expected: &str, got: &str) -> TypedNTError {
        TypedNTError::WrongStructId {
            expected: expected.into(),
            got: got.into(),
        }
    }

    /// The pvxs metadata baseline a `#[derive]`d Normative Type MUST carry
    /// for its resolved structure ID, in canonical pvxs order. Mirrors the
    /// fields pvxs `*::build()` always emits:
    /// - NTScalar / NTScalarArray (`nt.cpp:44-53`): `alarm`, `timeStamp`
    /// - NTEnum (`nt.cpp:121-131`): `alarm`, `timeStamp`, `display{description}`
    /// - NTTable (`nt.cpp:170-176`): `descriptor`, `alarm`, `timeStamp`
    ///   (`labels` + `value` are emitted by the derive ahead of these)
    ///
    /// Any other / empty structure ID has no NT baseline — the field list
    /// passes through unchanged.
    fn nt_meta_desc(struct_id: &str) -> ::std::vec::Vec<(&'static str, FieldDesc)> {
        use crate::nt::meta;
        match struct_id {
            "epics:nt/NTScalar:1.0" | "epics:nt/NTScalarArray:1.0" => {
                ::std::vec![
                    ("alarm", meta::alarm_desc()),
                    ("timeStamp", meta::time_desc())
                ]
            }
            "epics:nt/NTEnum:1.0" => ::std::vec![
                ("alarm", meta::alarm_desc()),
                ("timeStamp", meta::time_desc()),
                ("display", enum_display_desc()),
            ],
            "epics:nt/NTTable:1.0" => ::std::vec![
                ("descriptor", FieldDesc::Scalar(ScalarType::String)),
                ("alarm", meta::alarm_desc()),
                ("timeStamp", meta::time_desc()),
            ],
            _ => ::std::vec::Vec::new(),
        }
    }

    /// Default values matching [`nt_meta_desc`], used to fill any mandatory
    /// member the derive user did not declare.
    fn nt_meta_value(struct_id: &str) -> ::std::vec::Vec<(&'static str, PvField)> {
        use crate::nt::meta;
        match struct_id {
            "epics:nt/NTScalar:1.0" | "epics:nt/NTScalarArray:1.0" => ::std::vec![
                ("alarm", meta::alarm_default()),
                ("timeStamp", meta::time_default())
            ],
            "epics:nt/NTEnum:1.0" => ::std::vec![
                ("alarm", meta::alarm_default()),
                ("timeStamp", meta::time_default()),
                ("display", enum_display_default()),
            ],
            "epics:nt/NTTable:1.0" => ::std::vec![
                (
                    "descriptor",
                    PvField::Scalar(ScalarValue::String(String::new().into()))
                ),
                ("alarm", meta::alarm_default()),
                ("timeStamp", meta::time_default()),
            ],
            _ => ::std::vec::Vec::new(),
        }
    }

    /// pvxs NTEnum `display` sub-struct: just `description` (`nt.cpp:128-130`).
    fn enum_display_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: ::std::vec![("description".into(), FieldDesc::Scalar(ScalarType::String))],
        }
    }

    fn enum_display_default() -> PvField {
        let mut s = PvStructure::new("");
        s.fields.push((
            "description".into(),
            PvField::Scalar(ScalarValue::String(String::new().into())),
        ));
        PvField::Structure(s)
    }

    /// Merge the pvxs metadata baseline for `struct_id` into a derived
    /// descriptor's field list. Every non-baseline user field is kept in
    /// declaration order; then each mandatory member is emitted in canonical
    /// pvxs order — the user's own field when they declared one with the
    /// matching name, otherwise the default. The result therefore always
    /// contains the mandatory members for the claimed structure ID *by
    /// construction*, so a `#[derive]` cannot reintroduce a truncated
    /// normative type no matter which `#[nt(meta)]` fields the user omits.
    pub fn ensure_nt_meta_desc(
        struct_id: &str,
        user: ::std::vec::Vec<(String, FieldDesc)>,
    ) -> ::std::vec::Vec<(String, FieldDesc)> {
        let baseline = nt_meta_desc(struct_id);
        if baseline.is_empty() {
            return user;
        }
        let mut out: ::std::vec::Vec<(String, FieldDesc)> =
            ::std::vec::Vec::with_capacity(user.len() + baseline.len());
        for (n, d) in &user {
            if !baseline.iter().any(|(bn, _)| *bn == n.as_str()) {
                out.push((n.clone(), d.clone()));
            }
        }
        for (bn, bd) in &baseline {
            match user.iter().find(|(n, _)| n.as_str() == *bn) {
                Some((_, ud)) => out.push(((*bn).to_string(), ud.clone())),
                None => out.push(((*bn).to_string(), bd.clone())),
            }
        }
        out
    }

    /// Value-side counterpart of [`ensure_nt_meta_desc`].
    pub fn ensure_nt_meta_value(
        struct_id: &str,
        user: ::std::vec::Vec<(String, PvField)>,
    ) -> ::std::vec::Vec<(String, PvField)> {
        let baseline = nt_meta_value(struct_id);
        if baseline.is_empty() {
            return user;
        }
        let mut out: ::std::vec::Vec<(String, PvField)> =
            ::std::vec::Vec::with_capacity(user.len() + baseline.len());
        for (n, v) in &user {
            if !baseline.iter().any(|(bn, _)| *bn == n.as_str()) {
                out.push((n.clone(), v.clone()));
            }
        }
        for (bn, bv) in &baseline {
            match user.iter().find(|(n, _)| n.as_str() == *bn) {
                Some((_, uv)) => out.push(((*bn).to_string(), uv.clone())),
                None => out.push(((*bn).to_string(), bv.clone())),
            }
        }
        out
    }
}

fn get_i32(s: &PvStructure, name: &str) -> Option<i32> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::Int(v)) => Some(*v),
        PvField::Scalar(ScalarValue::Short(v)) => Some(*v as i32),
        PvField::Scalar(ScalarValue::Byte(v)) => Some(*v as i32),
        _ => None,
    }
}

fn get_i64(s: &PvStructure, name: &str) -> Option<i64> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::Long(v)) => Some(*v),
        PvField::Scalar(ScalarValue::Int(v)) => Some(*v as i64),
        _ => None,
    }
}

fn get_f32(s: &PvStructure, name: &str) -> Option<f32> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::Float(v)) => Some(*v),
        PvField::Scalar(ScalarValue::Double(v)) => Some(*v as f32),
        _ => None,
    }
}

fn get_f64(s: &PvStructure, name: &str) -> Option<f64> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::Double(v)) => Some(*v),
        PvField::Scalar(ScalarValue::Float(v)) => Some(*v as f64),
        PvField::Scalar(ScalarValue::Long(v)) => Some(*v as f64),
        PvField::Scalar(ScalarValue::Int(v)) => Some(*v as f64),
        _ => None,
    }
}

fn get_bool(s: &PvStructure, name: &str) -> Option<bool> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::Boolean(v)) => Some(*v),
        _ => None,
    }
}

fn get_str(s: &PvStructure, name: &str) -> Option<String> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::String(v)) => Some(v.as_str_lossy().into_owned()),
        _ => None,
    }
}

// ── Manual TypedNT impls for the primitive scalar wrappers.
//
// Most users will go through `#[derive(NTScalar)]` on a struct, but a
// bare scalar like `f64` is also useful when wrapping a single-value
// PV (e.g. `pvget_typed::<f64>`). The descriptor we emit is
// `epics:nt/NTScalar:1.0 { value: <T>, alarm, timeStamp }` — the same
// mandatory field set pvxs `NTScalar::build()` always emits
// (`nt.cpp:44-53`), produced by routing through the shared NT-baseline
// owner [`__rt::ensure_nt_meta_desc`] / [`__rt::ensure_nt_meta_value`]
// so the bare-scalar path cannot advertise a normative structure ID
// with a truncated field set.

/// Build the mandatory `epics:nt/NTScalar:1.0 { value: <st>, alarm,
/// timeStamp }` descriptor for a primitive scalar wrapper.
fn nt_scalar_root(value_field: FieldDesc) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: __rt::ensure_nt_meta_desc(
            "epics:nt/NTScalar:1.0",
            vec![("value".into(), value_field)],
        ),
    }
}

fn nt_scalar_value(s: &PvStructure) -> Result<&PvField, TypedNTError> {
    if !(s.struct_id.is_empty() || s.struct_id == "epics:nt/NTScalar:1.0") {
        return Err(TypedNTError::WrongStructId {
            expected: "epics:nt/NTScalar:1.0".into(),
            got: s.struct_id.clone(),
        });
    }
    s.get_field("value")
        .ok_or_else(|| TypedNTError::MissingField("value".into()))
}

macro_rules! impl_typed_nt_scalar {
    ($t:ty, $st:ident, $sv:ident) => {
        impl TypedNT for $t {
            fn descriptor() -> FieldDesc {
                nt_scalar_root(FieldDesc::Scalar(crate::pvdata::ScalarType::$st))
            }
            fn to_pv_field(&self) -> PvField {
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields = __rt::ensure_nt_meta_value(
                    "epics:nt/NTScalar:1.0",
                    vec![("value".into(), PvField::Scalar(ScalarValue::$sv(*self)))],
                );
                PvField::Structure(s)
            }
            fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
                match field {
                    PvField::Scalar(ScalarValue::$sv(v)) => Ok(*v),
                    PvField::Structure(s) => match nt_scalar_value(s)? {
                        PvField::Scalar(ScalarValue::$sv(v)) => Ok(*v),
                        other => Err(TypedNTError::WrongType {
                            field: "value".into(),
                            detail: format!("expected {} scalar, got {other:?}", stringify!($st)),
                        }),
                    },
                    other => Err(TypedNTError::WrongType {
                        field: "<root>".into(),
                        detail: format!("expected NTScalar wrapper, got {other:?}"),
                    }),
                }
            }
        }
    };
}

impl_typed_nt_scalar!(f64, Double, Double);
impl_typed_nt_scalar!(f32, Float, Float);
impl_typed_nt_scalar!(i64, Long, Long);
impl_typed_nt_scalar!(i32, Int, Int);
impl_typed_nt_scalar!(i16, Short, Short);
impl_typed_nt_scalar!(i8, Byte, Byte);
impl_typed_nt_scalar!(u64, ULong, ULong);
impl_typed_nt_scalar!(u32, UInt, UInt);
impl_typed_nt_scalar!(u16, UShort, UShort);
impl_typed_nt_scalar!(u8, UByte, UByte);
impl_typed_nt_scalar!(bool, Boolean, Boolean);

impl TypedNT for String {
    fn descriptor() -> FieldDesc {
        nt_scalar_root(FieldDesc::Scalar(crate::pvdata::ScalarType::String))
    }
    fn to_pv_field(&self) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields = __rt::ensure_nt_meta_value(
            "epics:nt/NTScalar:1.0",
            vec![(
                "value".into(),
                PvField::Scalar(ScalarValue::String(self.clone().into())),
            )],
        );
        PvField::Structure(s)
    }
    fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        match field {
            PvField::Scalar(ScalarValue::String(v)) => Ok(v.as_str_lossy().into_owned()),
            PvField::Structure(s) => match nt_scalar_value(s)? {
                PvField::Scalar(ScalarValue::String(v)) => Ok(v.as_str_lossy().into_owned()),
                other => Err(TypedNTError::WrongType {
                    field: "value".into(),
                    detail: format!("expected String scalar, got {other:?}"),
                }),
            },
            other => Err(TypedNTError::WrongType {
                field: "<root>".into(),
                detail: format!("expected NTScalar wrapper, got {other:?}"),
            }),
        }
    }
}

// ── NT Scalar-array wrappers (Vec<T> → NTScalarArray) ─────────────
//
// Round I-2 derive matrix runtime support. The `#[derive(NTScalarArray)]`
// macro emits a `TypedNT` impl that funnels element encoding through the
// helpers below — keeps the wire-format quirks in one place rather than
// inside every macro expansion.

fn nt_scalar_array_root(elem_type: crate::pvdata::ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields: __rt::ensure_nt_meta_desc(
            "epics:nt/NTScalarArray:1.0",
            vec![("value".into(), FieldDesc::ScalarArray(elem_type))],
        ),
    }
}

macro_rules! impl_typed_nt_scalar_array {
    // Element decode helper: the String variant now stores a `PvString`, so
    // take a lossy text view to land back in `Vec<String>`; numeric variants
    // are identity. (Encode is uniform `.into()` — String→PvString or
    // numeric identity — so only decode needs the per-variant split.)
    (@from_elem String, $x:expr) => {
        $x.as_str_lossy().into_owned()
    };
    (@from_elem $sv:ident, $x:expr) => {
        $x.clone()
    };
    ($t:ty, $st:ident, $sv:ident) => {
        impl TypedNT for ::std::vec::Vec<$t> {
            fn descriptor() -> FieldDesc {
                nt_scalar_array_root(crate::pvdata::ScalarType::$st)
            }
            fn to_pv_field(&self) -> PvField {
                let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
                let items: ::std::vec::Vec<ScalarValue> =
                    self.iter().map(|v| ScalarValue::$sv(v.clone().into())).collect();
                s.fields = __rt::ensure_nt_meta_value(
                    "epics:nt/NTScalarArray:1.0",
                    ::std::vec![("value".into(), PvField::ScalarArray(items))],
                );
                PvField::Structure(s)
            }
            fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
                let s = match field {
                    PvField::Structure(s) => s,
                    _ => {
                        return Err(TypedNTError::WrongType {
                            field: "<root>".into(),
                            detail: "expected NTScalarArray wrapper".into(),
                        });
                    }
                };
                if !(s.struct_id.is_empty() || s.struct_id == "epics:nt/NTScalarArray:1.0") {
                    return Err(TypedNTError::WrongStructId {
                        expected: "epics:nt/NTScalarArray:1.0".into(),
                        got: s.struct_id.clone(),
                    });
                }
                let items = match s.get_field("value") {
                    Some(PvField::ScalarArray(items)) => items,
                    _ => return Err(TypedNTError::MissingField("value".into())),
                };
                let mut out = ::std::vec::Vec::with_capacity(items.len());
                for v in items {
                    match v {
                        ScalarValue::$sv(x) => {
                            out.push(impl_typed_nt_scalar_array!(@from_elem $sv, x))
                        }
                        other => {
                            return Err(TypedNTError::WrongType {
                                field: "value[]".into(),
                                detail: format!(
                                    "expected {} element, got {other:?}",
                                    stringify!($t)
                                ),
                            });
                        }
                    }
                }
                Ok(out)
            }
        }
    };
}

impl_typed_nt_scalar_array!(f64, Double, Double);
impl_typed_nt_scalar_array!(f32, Float, Float);
impl_typed_nt_scalar_array!(i64, Long, Long);
impl_typed_nt_scalar_array!(i32, Int, Int);
impl_typed_nt_scalar_array!(i16, Short, Short);
impl_typed_nt_scalar_array!(i8, Byte, Byte);
impl_typed_nt_scalar_array!(u64, ULong, ULong);
impl_typed_nt_scalar_array!(u32, UInt, UInt);
impl_typed_nt_scalar_array!(u16, UShort, UShort);
impl_typed_nt_scalar_array!(u8, UByte, UByte);
impl_typed_nt_scalar_array!(String, String, String);

// ── NTEnum: index + choices[] ─────────────────────────────────────

/// NTEnum mirror — what a `#[derive(NTEnum)]` user-defined enum
/// reduces to on the wire after the macro discharges.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumValue {
    pub index: i32,
    pub choices: ::std::vec::Vec<String>,
}

impl TypedNT for EnumValue {
    fn descriptor() -> FieldDesc {
        let value_field = FieldDesc::Structure {
            struct_id: "enum_t".into(),
            fields: vec![
                (
                    "index".into(),
                    FieldDesc::Scalar(crate::pvdata::ScalarType::Int),
                ),
                (
                    "choices".into(),
                    FieldDesc::ScalarArray(crate::pvdata::ScalarType::String),
                ),
            ],
        };
        FieldDesc::Structure {
            struct_id: "epics:nt/NTEnum:1.0".into(),
            fields: __rt::ensure_nt_meta_desc(
                "epics:nt/NTEnum:1.0",
                vec![("value".into(), value_field)],
            ),
        }
    }

    fn to_pv_field(&self) -> PvField {
        let mut value_struct = PvStructure::new("enum_t");
        value_struct.fields.push((
            "index".into(),
            PvField::Scalar(ScalarValue::Int(self.index)),
        ));
        let choices_items: ::std::vec::Vec<ScalarValue> = self
            .choices
            .iter()
            .map(|c| ScalarValue::String(c.clone().into()))
            .collect();
        value_struct
            .fields
            .push(("choices".into(), PvField::ScalarArray(choices_items)));

        let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
        root.fields = __rt::ensure_nt_meta_value(
            "epics:nt/NTEnum:1.0",
            vec![("value".into(), PvField::Structure(value_struct))],
        );
        PvField::Structure(root)
    }

    fn from_pv_field(field: &PvField) -> Result<Self, TypedNTError> {
        let s = match field {
            PvField::Structure(s) => s,
            _ => {
                return Err(TypedNTError::WrongType {
                    field: "<root>".into(),
                    detail: "expected NTEnum wrapper".into(),
                });
            }
        };
        if !(s.struct_id.is_empty() || s.struct_id == "epics:nt/NTEnum:1.0") {
            return Err(TypedNTError::WrongStructId {
                expected: "epics:nt/NTEnum:1.0".into(),
                got: s.struct_id.clone(),
            });
        }
        let value_struct = match s.get_field("value") {
            Some(PvField::Structure(v)) => v,
            _ => return Err(TypedNTError::MissingField("value".into())),
        };
        let index = get_i32(value_struct, "index").unwrap_or(0);
        let choices = match value_struct.get_field("choices") {
            Some(PvField::ScalarArray(items)) => items
                .iter()
                .map(|v| match v {
                    ScalarValue::String(s) => Ok(s.as_str_lossy().into_owned()),
                    other => Err(TypedNTError::WrongType {
                        field: "choices[]".into(),
                        detail: format!("expected string, got {other:?}"),
                    }),
                })
                .collect::<Result<::std::vec::Vec<_>, _>>()?,
            Some(other) => {
                return Err(TypedNTError::WrongType {
                    field: "choices".into(),
                    detail: format!("expected scalar-array, got {other:?}"),
                });
            }
            None => ::std::vec::Vec::new(),
        };
        Ok(Self { index, choices })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_round_trip() {
        let v: f64 = 2.71;
        let field = v.to_pv_field();
        let back = f64::from_pv_field(&field).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn f64_descriptor_shape() {
        use crate::nt::meta;
        // pvxs NTScalar::build always emits value, alarm, timeStamp
        // (nt.cpp:44-53); the bare-scalar wrapper must carry the same
        // mandatory members, not a truncated value-only structure.
        match f64::descriptor() {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, vec!["value", "alarm", "timeStamp"]);
                assert!(matches!(
                    fields[0].1,
                    FieldDesc::Scalar(crate::pvdata::ScalarType::Double)
                ));
                assert_eq!(&fields[1].1, &meta::alarm_desc());
                assert_eq!(&fields[2].1, &meta::time_desc());
            }
            other => panic!("expected Structure descriptor, got {other:?}"),
        }
    }

    #[test]
    fn primitive_scalar_value_carries_mandatory_metadata() {
        use crate::nt::meta;
        let PvField::Structure(s) = (1.5f64).to_pv_field() else {
            panic!("expected NTScalar structure value");
        };
        assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
        let names: Vec<&str> = s.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["value", "alarm", "timeStamp"]);
        assert_eq!(s.get_field("alarm"), Some(&meta::alarm_default()));
        assert_eq!(s.get_field("timeStamp"), Some(&meta::time_default()));
    }

    #[test]
    fn primitive_array_descriptor_carries_mandatory_metadata() {
        use crate::nt::meta;
        match <Vec<f64>>::descriptor() {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
                let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, vec!["value", "alarm", "timeStamp"]);
                assert!(matches!(
                    fields[0].1,
                    FieldDesc::ScalarArray(crate::pvdata::ScalarType::Double)
                ));
                assert_eq!(&fields[1].1, &meta::alarm_desc());
                assert_eq!(&fields[2].1, &meta::time_desc());
            }
            other => panic!("expected Structure descriptor, got {other:?}"),
        }
    }

    #[test]
    fn primitive_enum_descriptor_carries_full_ntenum_baseline() {
        use crate::nt::meta;
        // pvxs NTEnum::build: value, alarm, timeStamp, display{description}
        // (nt.cpp:121-131).
        match EnumValue::descriptor() {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTEnum:1.0");
                let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, vec!["value", "alarm", "timeStamp", "display"]);
                assert_eq!(&fields[1].1, &meta::alarm_desc());
                assert_eq!(&fields[2].1, &meta::time_desc());
            }
            other => panic!("expected Structure descriptor, got {other:?}"),
        }
        // Round-trip still works through the enriched wrapper.
        let v = EnumValue {
            index: 2,
            choices: vec!["a".into(), "b".into(), "c".into()],
        };
        assert_eq!(EnumValue::from_pv_field(&v.to_pv_field()).unwrap(), v);
    }

    #[test]
    fn alarm_round_trip() {
        let a = Alarm {
            severity: 2,
            status: 7,
            message: "hi".into(),
        };
        let field = TypedNT::to_pv_field(&a);
        let back = <Alarm as TypedNT>::from_pv_field(&field).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn from_wrong_struct_id_rejected() {
        let mut s = PvStructure::new("epics:nt/NTTable:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        let err = f64::from_pv_field(&PvField::Structure(s)).unwrap_err();
        assert!(matches!(err, TypedNTError::WrongStructId { .. }));
    }

    #[test]
    fn missing_value_rejected() {
        let s = PvStructure::new("epics:nt/NTScalar:1.0");
        let err = f64::from_pv_field(&PvField::Structure(s)).unwrap_err();
        assert!(matches!(err, TypedNTError::MissingField(_)));
    }

    #[test]
    fn wrong_scalar_type_rejected() {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields.push((
            "value".into(),
            PvField::Scalar(ScalarValue::String("oops".into())),
        ));
        let err = f64::from_pv_field(&PvField::Structure(s)).unwrap_err();
        assert!(matches!(err, TypedNTError::WrongType { .. }));
    }
}
