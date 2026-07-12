//! pvxs `Value::copyOut` / `Value::as<T>()` — the scalar conversion every
//! `record._options.*` reader goes through.
//!
//! pvxs never type-*matches* a pvRequest option: it asks the field to
//! convert itself. `Value::as<T>()` maps `T` to one storage class
//! (`StorageMap`, `data.h:58-80`: signed → `int64_t`, unsigned →
//! `uint64_t`, floating → `double`, `bool`, `std::string`) and calls
//! `Value::copyOut(ptr, StoreType)` (`data.cpp:418-499`), which switches on
//! the field's *storage* and C-casts it into the requested class
//! (`copyOutScalar`, `data.cpp:400-416`). A conversion that has no arm —
//! array storage into a scalar, a struct (`StoreType::Null`), a string that
//! will not parse — falls through to `throw NoConvert` (`data.cpp:499`).
//!
//! Two outcomes follow from the SAME conversion, and the caller picks:
//!
//! * `bool Value::as(T& out)` (`data.h:634-647`) swallows the throw and
//!   answers `false` — pvxs's `pipeline.as(v)` / `queueSize.as(qSize)`.
//!   Callers map that to [`Result::ok`].
//! * `T Value::as()` (`data.h:625-631`) lets `NoConvert` escape. Inside a
//!   server command handler there is no catch between it and
//!   `conn.cpp:277-282`, which logs and does `bev.reset()` — the circuit
//!   drops. Callers propagate the [`NoConvert`].
//!
//! Hand-rolling one match arm per [`ScalarValue`] variant at each option is
//! what produced the divergences this module closes (R9-33/34/35,
//! R10-31/32): each copy accepted a different subset of the storage classes
//! pvxs converts, and none of them could throw.

use std::borrow::Cow;

use super::scalar::{ScalarType, ScalarValue};
use super::structure::PvField;

/// pvxs `NoConvert` (`data.cpp:499`, `pvxs/data.h`) — the field's storage
/// cannot be coerced into the requested class.
///
/// Whether this is fatal is the CALLER's choice, exactly as in pvxs: the
/// `as(T&)` form turns it into `false` (`Value::tryCopyOut`), the `as<T>()`
/// form throws it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoConvert(String);

impl NoConvert {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    /// The pvxs-shaped reason text (`"Can't extract … as …"`,
    /// `"Invalid input : …"`, `"Extraneous characters after integer: …"`).
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NoConvert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoConvert {}

/// pvxs `impl::FieldStorage::code` — the storage class a field's value
/// actually lives in (`TypeCode::storedAs`, `type.cpp:73-99`).
///
/// This is the discriminant `Value::copyOut` switches on (`data.cpp:424`),
/// and it is NOT the field's type code: every array (`Int32A`, `StringA`,
/// `StructA`, …) stores as [`Store::Array`], a struct stores as
/// [`Store::Null`], and a union / any stores as [`Store::Compound`] holding
/// the selected member.
enum Store<'a> {
    /// `StoreType::Bool`.
    Bool(bool),
    /// `StoreType::Integer` — every signed integer widens to `int64_t`.
    Integer(i64),
    /// `StoreType::UInteger` — every unsigned integer widens to `uint64_t`.
    UInteger(u64),
    /// `StoreType::Real` — `float`/`double` widen to `double`.
    Real(f64),
    /// `StoreType::String`.
    Str(Cow<'a, str>),
    /// `StoreType::Array` — `copyOut` has no scalar arm for it
    /// (`data.cpp:466-476`), so every scalar target raises `NoConvert`.
    Array,
    /// `StoreType::Compound` — a union / any. `copyOut` "automagic
    /// derefs" the selected member and delegates (`data.cpp:478-492`);
    /// an unselected one has no arm and raises `NoConvert`.
    Compound(Option<&'a PvField>),
    /// `StoreType::Null` — a struct (`type.cpp:78-79`), or the port's
    /// explicit empty value. No scalar arm (`data.cpp:495-496`).
    Null,
}

/// pvxs `Kind` (`data.h:140-147`) — the type code's class nibble
/// (`code & 0xe0`), which is what a source's `switch(fld.type().kind())`
/// dispatches on. Distinct from [`Store`]: `Int32A` is `Kind::Integer` but
/// `StoreType::Array`, which is exactly why an array-typed
/// `record._options.DBE` reaches pvxs's `fld.as<uint8_t>()` and throws
/// (`ioc/singlesource.cpp:134-136`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `Bool` / `BoolA`.
    Bool,
    /// Every signed and unsigned integer, scalar or array.
    Integer,
    /// `Float32`/`Float64` and their arrays.
    Real,
    /// `String` / `StringA`.
    String,
    /// `Struct`, `Union`, `Any` and their arrays (`0x80` class).
    Compound,
    /// No type code — the port's [`PvField::Null`], and an untyped empty
    /// [`PvField::ScalarArray`] (see [`kind`]).
    Null,
}

/// pvxs `Value::type().kind()` — the field's type class.
///
/// [`PvField::ScalarArray`] is the port's untyped in-memory array form; the
/// wire decoder always produces a typed [`PvField::ScalarArrayTyped`], so an
/// EMPTY untyped array has no wire counterpart and reports [`Kind::Null`],
/// landing in the same "no arm" case a struct does.
pub fn kind(f: &PvField) -> Kind {
    match f {
        PvField::Scalar(sv) => scalar_type_kind(sv.scalar_type()),
        PvField::ScalarArrayTyped(a) => scalar_type_kind(a.scalar_type()),
        PvField::ScalarArray(v) => v
            .first()
            .map_or(Kind::Null, |sv| scalar_type_kind(sv.scalar_type())),
        PvField::Structure(_)
        | PvField::StructureArray(_)
        | PvField::Union { .. }
        | PvField::UnionArray(_)
        | PvField::Variant(_)
        | PvField::VariantArray(_) => Kind::Compound,
        PvField::Null => Kind::Null,
    }
}

fn scalar_type_kind(st: ScalarType) -> Kind {
    match st {
        ScalarType::Boolean => Kind::Bool,
        ScalarType::Byte
        | ScalarType::Short
        | ScalarType::Int
        | ScalarType::Long
        | ScalarType::UByte
        | ScalarType::UShort
        | ScalarType::UInt
        | ScalarType::ULong => Kind::Integer,
        ScalarType::Float | ScalarType::Double => Kind::Real,
        ScalarType::String => Kind::String,
    }
}

/// pvxs `TypeCode::storedAs()` (`type.cpp:73-99`) — classify a field by the
/// storage `copyOut` reads, not by its type code.
fn store_of(f: &PvField) -> Store<'_> {
    match f {
        PvField::Scalar(sv) => store_of_scalar(sv),
        // Every array type stores as `StoreType::Array` (`type.cpp:75-76`),
        // whatever its element kind.
        PvField::ScalarArray(_)
        | PvField::ScalarArrayTyped(_)
        | PvField::StructureArray(_)
        | PvField::UnionArray(_)
        | PvField::VariantArray(_) => Store::Array,
        // `TypeCode::Struct` stores as `StoreType::Null` (`type.cpp:78-79`).
        PvField::Structure(_) => Store::Null,
        // Union / any store the SELECTED member as a `Value`
        // (`StoreType::Compound`). A null selector leaves it empty, which
        // `copyOut`'s `else if(src)` guard rejects (`data.cpp:485-490`).
        PvField::Union {
            selector, value, ..
        } => Store::Compound(
            (*selector >= 0 && !matches!(**value, PvField::Null)).then_some(&**value),
        ),
        PvField::Variant(v) => {
            Store::Compound((!matches!(v.value, PvField::Null)).then_some(&v.value))
        }
        PvField::Null => Store::Null,
    }
}

fn store_of_scalar(sv: &ScalarValue) -> Store<'_> {
    match sv {
        ScalarValue::Boolean(b) => Store::Bool(*b),
        ScalarValue::Byte(n) => Store::Integer(i64::from(*n)),
        ScalarValue::Short(n) => Store::Integer(i64::from(*n)),
        ScalarValue::Int(n) => Store::Integer(i64::from(*n)),
        ScalarValue::Long(n) => Store::Integer(*n),
        ScalarValue::UByte(n) => Store::UInteger(u64::from(*n)),
        ScalarValue::UShort(n) => Store::UInteger(u64::from(*n)),
        ScalarValue::UInt(n) => Store::UInteger(u64::from(*n)),
        ScalarValue::ULong(n) => Store::UInteger(*n),
        ScalarValue::Float(n) => Store::Real(f64::from(*n)),
        ScalarValue::Double(n) => Store::Real(*n),
        ScalarValue::String(s) => Store::Str(s.as_str_lossy()),
    }
}

/// pvxs `throw NoConvert(SB()<<"Can't extract "<<this->type()<<" as "<<type)`
/// (`data.cpp:499`) — the storage has no arm for the requested class.
fn no_scalar_arm(f: &PvField, target: &str) -> NoConvert {
    let src = match f {
        PvField::Scalar(sv) => format!("{:?}", sv.scalar_type()),
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => "scalar array".into(),
        PvField::Structure(_) => "structure".into(),
        PvField::StructureArray(_) => "structure array".into(),
        PvField::Union { .. } => "union".into(),
        PvField::UnionArray(_) => "union array".into(),
        PvField::Variant(_) => "any".into(),
        PvField::VariantArray(_) => "any array".into(),
        PvField::Null => "null".into(),
    };
    NoConvert::new(format!("Can't extract {src} as {target}"))
}

/// pvxs `Value::as<bool>()` / `Value::as(bool&)` — `copyOut` into
/// `StoreType::Bool`.
///
/// Bool passes through; every integer and real converts by C's
/// `bool(src)`, i.e. non-zero (`copyOutScalar`, `data.cpp:405`); a string is
/// accepted ONLY as the exact tokens `"true"` / `"false"` — no trim, case
/// sensitive (`data.cpp:466-469`). Anything else is [`NoConvert`].
pub fn as_bool(f: &PvField) -> Result<bool, NoConvert> {
    match store_of(f) {
        Store::Bool(b) => Ok(b),
        Store::Integer(n) => Ok(n != 0),
        Store::UInteger(n) => Ok(n != 0),
        // C's `bool(double)` is `src != 0.0`, so NaN is true.
        Store::Real(n) => Ok(n != 0.0),
        Store::Str(s) => match s.as_ref() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(NoConvert::new(format!("Can't extract \"{s}\" as bool"))),
        },
        Store::Compound(Some(inner)) => as_bool(inner),
        Store::Array | Store::Compound(None) | Store::Null => Err(no_scalar_arm(f, "bool")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::{PvStructure, TypedScalarArray, VariantValue};

    #[test]
    fn as_bool_converts_every_numeric_storage_class() {
        // pvxs `copyOutScalar` → `bool(src)`: non-zero is true, for EVERY
        // integer and real storage class, not just Boolean.
        for (f, want) in [
            (PvField::Scalar(ScalarValue::Boolean(true)), true),
            (PvField::Scalar(ScalarValue::Boolean(false)), false),
            (PvField::Scalar(ScalarValue::Int(1)), true),
            (PvField::Scalar(ScalarValue::Int(-3)), true),
            (PvField::Scalar(ScalarValue::Int(0)), false),
            (PvField::Scalar(ScalarValue::ULong(9)), true),
            (PvField::Scalar(ScalarValue::Float(1.0)), true),
            (PvField::Scalar(ScalarValue::Float(0.0)), false),
            (PvField::Scalar(ScalarValue::Double(1.0)), true),
            (PvField::Scalar(ScalarValue::Double(0.0)), false),
            (PvField::Scalar(ScalarValue::Double(-0.5)), true),
        ] {
            assert_eq!(as_bool(&f), Ok(want), "{f:?}");
        }
        // C `bool(NaN)` is `NaN != 0.0` → true.
        assert_eq!(
            as_bool(&PvField::Scalar(ScalarValue::Double(f64::NAN))),
            Ok(true)
        );
    }

    #[test]
    fn as_bool_string_takes_only_the_exact_tokens() {
        assert_eq!(
            as_bool(&PvField::Scalar(ScalarValue::String("true".into()))),
            Ok(true)
        );
        assert_eq!(
            as_bool(&PvField::Scalar(ScalarValue::String("false".into()))),
            Ok(false)
        );
        // pvxs `data.cpp:466-469` has no other string arm: no case folding,
        // no trim, no "1"/"yes"/"on".
        for s in ["True", "TRUE", " true", "1", "0", "yes", "no", "on", ""] {
            assert!(
                as_bool(&PvField::Scalar(ScalarValue::String(s.into()))).is_err(),
                "{s:?} must be NoConvert"
            );
        }
    }

    #[test]
    fn as_bool_has_no_arm_for_array_struct_or_empty_union() {
        // Array storage: `copyOut`'s Array case only serves an Array target.
        assert!(
            as_bool(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
                vec![1].into()
            )))
            .is_err()
        );
        assert!(as_bool(&PvField::ScalarArray(vec![ScalarValue::Boolean(true)])).is_err());
        // Struct storage is `StoreType::Null`.
        assert!(as_bool(&PvField::Structure(PvStructure::new(""))).is_err());
        assert!(as_bool(&PvField::Null).is_err());
        // An unselected union has no value to delegate to.
        assert!(
            as_bool(&PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            })
            .is_err()
        );
    }

    #[test]
    fn as_bool_derefs_a_selected_union_or_variant() {
        // pvxs `copyOut` Compound arm: "automagic deref and delegate assign".
        assert_eq!(
            as_bool(&PvField::Union {
                selector: 0,
                variant_name: "v".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Int(7))),
            }),
            Ok(true)
        );
        assert_eq!(
            as_bool(&PvField::Variant(Box::new(VariantValue::scalar(
                ScalarValue::Double(0.0)
            )))),
            Ok(false)
        );
    }

    #[test]
    fn kind_is_the_type_class_not_the_storage() {
        // `Int32A & 0xe0` is `Kind::Integer` even though it STORES as an
        // array — the divergence R9-35 turns on.
        assert_eq!(
            kind(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
                vec![1].into()
            ))),
            Kind::Integer
        );
        assert_eq!(
            kind(&PvField::ScalarArrayTyped(TypedScalarArray::String(
                vec!["VALUE".into()].into()
            ))),
            Kind::String
        );
        assert_eq!(
            kind(&PvField::ScalarArrayTyped(TypedScalarArray::Boolean(
                vec![true].into()
            ))),
            Kind::Bool
        );
        assert_eq!(
            kind(&PvField::ScalarArrayTyped(TypedScalarArray::Double(
                vec![1.0].into()
            ))),
            Kind::Real
        );
        assert_eq!(kind(&PvField::Scalar(ScalarValue::UInt(1))), Kind::Integer);
        assert_eq!(
            kind(&PvField::Structure(PvStructure::new(""))),
            Kind::Compound
        );
        assert_eq!(kind(&PvField::Null), Kind::Null);
        // Untyped, empty: no element to take the class from.
        assert_eq!(kind(&PvField::ScalarArray(Vec::new())), Kind::Null);
    }
}
