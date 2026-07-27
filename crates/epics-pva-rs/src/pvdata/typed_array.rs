//! Typed scalar arrays for zero-copy-friendly encode / decode.
//!
//! Background
//! ----------
//! [`crate::pvdata::PvField::ScalarArray`] carries values as
//! `Vec<ScalarValue>` — an enum-tagged list. For an `f64` array of N
//! elements that costs `N * 24 bytes` (enum tag + payload + alignment
//! padding for the largest variant) instead of `N * 8 bytes`, and the
//! wire-encoding path has to walk every element through a match arm
//! and a per-element write. pvxs uses ref-counted typed arrays
//! (`shared_array<T>`) which let the encode path collapse to a single
//! `memcpy` when the host endian matches the wire endian.
//!
//! [`TypedScalarArray`] is the matching representation: a sum type
//! over `Arc<[T]>` for each fixed-width PVA primitive, plus
//! `Arc<[String]>` and `Arc<[bool]>` for the variable-length / non-POD
//! variants. The key properties:
//!
//! - **`Arc<[T]>` (= reference-counted contiguous array)**: cloning
//!   the array bumps a refcount instead of allocating + memcpying N×T
//!   bytes. Subscriber fan-out (one upstream MONITOR event delivered
//!   to N downstream subscribers) collapses to N refcount bumps, not
//!   N copies.
//! - **`bytemuck::cast_slice`-friendly**: `T: Pod` for every numeric
//!   variant, so encode can take `&[T]` → `&[u8]` and call
//!   `Vec::extend_from_slice` once. LLVM lowers that to SIMD memcpy.
//! - **Endian round-trip**: when wire endian == host endian we emit
//!   the bytes directly. When they differ we still avoid the
//!   `Vec<ScalarValue>` intermediate — we walk the typed slice with
//!   `to_be_bytes` / `to_le_bytes` (one byte-swap per element, no
//!   enum match).
//!
//! Backwards compat: [`PvField::ScalarArray`](crate::pvdata::PvField::ScalarArray) still exists as the
//! generic catch-all. New code should prefer the typed constructors
//! ([`PvField::scalar_array_double`](crate::pvdata::PvField::scalar_array_double), etc.) to avoid the
//! `Vec<ScalarValue>` blowup. Encoders / decoders accept both.

use std::fmt;
use std::sync::Arc;

use epics_base_rs::types::PvString;

use super::scalar::{ScalarType, ScalarValue};

/// Typed, reference-counted scalar array. Each variant wraps an
/// `Arc<[T]>` so cloning is O(1).
///
/// Choose variants directly when the source data is already typed
/// (e.g. a Rust `Vec<f64>` from numerical code). For untyped /
/// runtime-discovered shapes the legacy
/// [`crate::pvdata::PvField::ScalarArray`] path still works — the
/// encoder will fall back to the per-element loop in that case.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedScalarArray {
    Boolean(Arc<[bool]>),
    Byte(Arc<[i8]>),
    UByte(Arc<[u8]>),
    Short(Arc<[i16]>),
    UShort(Arc<[u16]>),
    Int(Arc<[i32]>),
    UInt(Arc<[u32]>),
    Long(Arc<[i64]>),
    ULong(Arc<[u64]>),
    Float(Arc<[f32]>),
    Double(Arc<[f64]>),
    String(Arc<[PvString]>),
}

impl TypedScalarArray {
    /// The element [`ScalarType`] this array carries.
    pub fn scalar_type(&self) -> ScalarType {
        match self {
            Self::Boolean(_) => ScalarType::Boolean,
            Self::Byte(_) => ScalarType::Byte,
            Self::UByte(_) => ScalarType::UByte,
            Self::Short(_) => ScalarType::Short,
            Self::UShort(_) => ScalarType::UShort,
            Self::Int(_) => ScalarType::Int,
            Self::UInt(_) => ScalarType::UInt,
            Self::Long(_) => ScalarType::Long,
            Self::ULong(_) => ScalarType::ULong,
            Self::Float(_) => ScalarType::Float,
            Self::Double(_) => ScalarType::Double,
            Self::String(_) => ScalarType::String,
        }
    }

    /// Length in elements.
    pub fn len(&self) -> usize {
        match self {
            Self::Boolean(a) => a.len(),
            Self::Byte(a) => a.len(),
            Self::UByte(a) => a.len(),
            Self::Short(a) => a.len(),
            Self::UShort(a) => a.len(),
            Self::Int(a) => a.len(),
            Self::UInt(a) => a.len(),
            Self::Long(a) => a.len(),
            Self::ULong(a) => a.len(),
            Self::Float(a) => a.len(),
            Self::Double(a) => a.len(),
            Self::String(a) => a.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build a typed array from the legacy `Vec<ScalarValue>`
    /// representation. Returns `None` when the input has mixed types
    /// or when any element doesn't match the requested
    /// [`ScalarType`]; callers can then fall back to the slow path.
    pub fn from_scalar_values(values: &[ScalarValue], st: ScalarType) -> Option<Self> {
        match st {
            ScalarType::Boolean => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Boolean(b) => Some(*b),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Boolean(v.into())),
            ScalarType::Byte => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Byte(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Byte(v.into())),
            ScalarType::UByte => values
                .iter()
                .map(|v| match v {
                    ScalarValue::UByte(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::UByte(v.into())),
            ScalarType::Short => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Short(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Short(v.into())),
            ScalarType::UShort => values
                .iter()
                .map(|v| match v {
                    ScalarValue::UShort(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::UShort(v.into())),
            ScalarType::Int => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Int(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Int(v.into())),
            ScalarType::UInt => values
                .iter()
                .map(|v| match v {
                    ScalarValue::UInt(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::UInt(v.into())),
            ScalarType::Long => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Long(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Long(v.into())),
            ScalarType::ULong => values
                .iter()
                .map(|v| match v {
                    ScalarValue::ULong(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::ULong(v.into())),
            ScalarType::Float => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Float(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Float(v.into())),
            ScalarType::Double => values
                .iter()
                .map(|v| match v {
                    ScalarValue::Double(x) => Some(*x),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::Double(v.into())),
            ScalarType::String => values
                .iter()
                .map(|v| match v {
                    ScalarValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|v| Self::String(v.into())),
        }
    }

    /// Lossy conversion back to the legacy `Vec<ScalarValue>` for
    /// callers that haven't migrated. Allocates one `Vec` of length
    /// `self.len()` — explicitly NOT zero-copy. Use the typed
    /// accessors (`as_doubles()` etc.) when you can.
    pub fn to_scalar_values(&self) -> Vec<ScalarValue> {
        match self {
            Self::Boolean(a) => a.iter().map(|x| ScalarValue::Boolean(*x)).collect(),
            Self::Byte(a) => a.iter().map(|x| ScalarValue::Byte(*x)).collect(),
            Self::UByte(a) => a.iter().map(|x| ScalarValue::UByte(*x)).collect(),
            Self::Short(a) => a.iter().map(|x| ScalarValue::Short(*x)).collect(),
            Self::UShort(a) => a.iter().map(|x| ScalarValue::UShort(*x)).collect(),
            Self::Int(a) => a.iter().map(|x| ScalarValue::Int(*x)).collect(),
            Self::UInt(a) => a.iter().map(|x| ScalarValue::UInt(*x)).collect(),
            Self::Long(a) => a.iter().map(|x| ScalarValue::Long(*x)).collect(),
            Self::ULong(a) => a.iter().map(|x| ScalarValue::ULong(*x)).collect(),
            Self::Float(a) => a.iter().map(|x| ScalarValue::Float(*x)).collect(),
            Self::Double(a) => a.iter().map(|x| ScalarValue::Double(*x)).collect(),
            Self::String(a) => a.iter().map(|x| ScalarValue::String(x.clone())).collect(),
        }
    }

    /// Typed view as `&[f64]` (or `None` if this isn't a Double
    /// array). pvxs `shared_array<double>` accessor analogue.
    pub fn as_doubles(&self) -> Option<&[f64]> {
        match self {
            Self::Double(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_floats(&self) -> Option<&[f32]> {
        match self {
            Self::Float(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_ints(&self) -> Option<&[i32]> {
        match self {
            Self::Int(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_longs(&self) -> Option<&[i64]> {
        match self {
            Self::Long(a) => Some(a),
            _ => None,
        }
    }
}

impl fmt::Display for TypedScalarArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Reuse the legacy formatter to keep diagnostic output consistent.
        let v = self.to_scalar_values();
        write!(f, "[")?;
        for (i, sv) in v.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{sv}")?;
        }
        write!(f, "]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_clone_is_refcount_only() {
        let a = TypedScalarArray::Double(Arc::from(vec![1.0, 2.0, 3.0]));
        let b = a.clone();
        // Both should share the same allocation.
        if let (TypedScalarArray::Double(x), TypedScalarArray::Double(y)) = (&a, &b) {
            assert_eq!(Arc::strong_count(x), 2);
            assert_eq!(x.as_ptr(), y.as_ptr());
        } else {
            panic!("variant lost");
        }
    }

    #[test]
    fn from_scalar_values_homogeneous_double() {
        let v = vec![
            ScalarValue::Double(1.0),
            ScalarValue::Double(2.0),
            ScalarValue::Double(3.0),
        ];
        let arr = TypedScalarArray::from_scalar_values(&v, ScalarType::Double).unwrap();
        assert_eq!(arr.as_doubles().unwrap(), &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn from_scalar_values_mixed_returns_none() {
        let v = vec![ScalarValue::Double(1.0), ScalarValue::Int(2)];
        assert!(TypedScalarArray::from_scalar_values(&v, ScalarType::Double).is_none());
    }

    #[test]
    fn round_trip_via_scalar_values() {
        let arr = TypedScalarArray::Double(Arc::from(vec![1.5, 2.5, 3.5]));
        let v = arr.to_scalar_values();
        let arr2 = TypedScalarArray::from_scalar_values(&v, ScalarType::Double).unwrap();
        assert_eq!(arr.as_doubles().unwrap(), arr2.as_doubles().unwrap());
    }
}
