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
//! * `T Value::as()` (`data.h:626-631`) lets `NoConvert` escape. Inside a
//!   server command handler there is no catch between it and
//!   `conn.cpp:277-282`, which logs and does `bev.reset()` — the circuit
//!   drops. Callers propagate the [`NoConvert`].
//!
//! Hand-rolling one match arm per [`ScalarValue`] variant at each option is
//! what produced the divergences this module closes (R9-33/34/35,
//! R10-31/32): each copy accepted a different subset of the storage classes
//! pvxs converts, and none of them could throw.

use std::borrow::Cow;

use epics_base_rs::runtime::stdlib::{c_isspace, strtod, strtoll_errno, strtoull_errno};

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
/// dispatches on. Distinct from `Store`: `Int32A` is `Kind::Integer` but
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

/// pvxs `Value::as<uint64_t>()` / `Value::as(uint64_t&)` — `copyOut` into
/// `StoreType::UInteger`.
///
/// Every storage class C-casts to `uint64_t` (`copyOutScalar`,
/// `data.cpp:404`): a bool becomes 0/1, a signed integer WRAPS
/// (`uint64_t(int64_t(-1))` is `u64::MAX`), a real truncates toward zero,
/// and a string goes through [`parse_to_u64`] — pvxs `parseTo<uint64_t>`,
/// which is `stoull(s, &idx, 0)`, i.e. BASE 0 (`util.cpp:786-799`).
///
/// DEVIATION, deliberate and recorded: `uint64_t(double)` for a negative,
/// NaN, or overflowing real is undefined behavior in C++, so the port takes
/// Rust's defined saturating cast (`-1.0` and `NaN` → 0) rather than
/// reproducing an undefined result.
pub fn as_u64(f: &PvField) -> Result<u64, NoConvert> {
    match store_of(f) {
        Store::Bool(b) => Ok(u64::from(b)),
        Store::Integer(n) => Ok(n as u64),
        Store::UInteger(n) => Ok(n),
        Store::Real(n) => Ok(n as u64),
        Store::Str(s) => parse_to_u64(&s),
        Store::Compound(Some(inner)) => as_u64(inner),
        Store::Array | Store::Compound(None) | Store::Null => Err(no_scalar_arm(f, "uint64")),
    }
}

/// pvxs `Value::as<uint32_t>()` — [`as_u64`] narrowed by
/// `StoreTransform<uint32_t>::out` (`data.h:111-116, 626-631`), which binds the
/// `uint64_t` store to a `const uint32_t&` and so TRUNCATES. `ackAny = -1`
/// therefore reaches `op->ackAt` as `0xFFFF_FFFF`, not as 1 or 0.
pub fn as_u32(f: &PvField) -> Result<u32, NoConvert> {
    as_u64(f).map(|v| v as u32)
}

/// pvxs `Value::as<uint8_t>()` — [`as_u64`] truncated to the low byte, the
/// form `ioc/singlesource.cpp:135` uses for `record._options.DBE`.
pub fn as_u8(f: &PvField) -> Result<u8, NoConvert> {
    as_u64(f).map(|v| v as u8)
}

/// pvxs `Value::as<std::string>()` / `Value::as(std::string&)` — `copyOut`
/// into `StoreType::String`.
///
/// A string passes through; a bool renders as the exact tokens
/// `"true"`/`"false"` (`data.cpp:438`); every integer and real renders through
/// `SB()<<src` (`copyOutScalar`, `data.cpp:409`). Array, struct and unselected
/// union storage have no arm → [`NoConvert`]; a selected union derefs.
pub fn as_string(f: &PvField) -> Result<String, NoConvert> {
    match store_of(f) {
        Store::Bool(b) => Ok(if b { "true".into() } else { "false".into() }),
        Store::Integer(n) => Ok(n.to_string()),
        Store::UInteger(n) => Ok(n.to_string()),
        Store::Real(n) => Ok(super::fmt::render_real(n)),
        Store::Str(s) => Ok(s.into_owned()),
        Store::Compound(Some(inner)) => as_string(inner),
        Store::Array | Store::Compound(None) | Store::Null => Err(no_scalar_arm(f, "string")),
    }
}

/// pvxs `parseTo<uint64_t>` (`util.cpp:786-799`) — `std::stoull(s, &idx, 0)`
/// followed by a trailing-whitespace skip and an "extraneous characters"
/// check. `std::stoull` is libcom's [`strtoull_errno`] at base 0: a leading
/// `-` negates (`"-1"` is `u64::MAX`), `0x`/`0b`/leading-`0` pick the radix,
/// and overflow is `std::out_of_range` → `NoConvert`.
pub fn parse_to_u64(s: &str) -> Result<u64, NoConvert> {
    parse_base0(s, "integer", |b| strtoull_errno(b, 0))
}

/// pvxs `parseTo<int64_t>` (`util.cpp:802-816`) — `std::stoll(s, &idx, 0)`,
/// the same scan as [`parse_to_u64`] with a signed range. Its extraneous text
/// says "unsigned", as pvxs's does.
pub fn parse_to_i64(s: &str) -> Result<i64, NoConvert> {
    parse_base0(s, "unsigned", |b| strtoll_errno(b, 0))
}

/// pvxs `parseTo<double>` (`util.cpp:768-783`) — `std::stod(s, &idx)`, which
/// is glibc [`strtod`] (hex floats, `inf`/`nan`) with ERANGE, overflow or a
/// tiny inexact result, as `std::out_of_range`; then the same trailing check.
pub fn parse_to_f64(s: &str) -> Result<f64, NoConvert> {
    parse_base0(s, "double", |b| {
        let r = strtod(b);
        (!r.erange).then_some((r.value, r.len))
    })
}

/// `parseTo` around its `std::stoll`/`std::stoull`/`std::stod` `scan`: `None` (ERANGE)
/// is `std::out_of_range`, raised before the trailing check; nothing
/// converted is `std::invalid_argument`.
fn parse_base0<T>(
    s: &str,
    what: &str,
    scan: impl FnOnce(&[u8]) -> Option<(T, usize)>,
) -> Result<T, NoConvert> {
    let (v, end) =
        scan(s.as_bytes()).ok_or_else(|| NoConvert::new(format!("Out of range : \"{s}\"")))?;
    if end == 0 {
        return Err(NoConvert::new(format!("Invalid input : \"{s}\"")));
    }
    if !s.as_bytes()[end..].iter().all(|&c| c_isspace(c as char)) {
        return Err(NoConvert::new(format!(
            "Extraneous characters after {what}: \"{s}\""
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::{PvStructure, TypedScalarArray, VariantValue};

    /// `SB()<<src` (`data.cpp:409`): the stream default, `%g` at six digits.
    #[test]
    fn as_string_renders_a_real_as_the_stream_default() {
        for (v, want) in [(0.1 + 0.2, "0.3"), (1e20, "1e+20"), (-f64::NAN, "-nan")] {
            assert_eq!(
                as_string(&PvField::Scalar(ScalarValue::Double(v))).unwrap(),
                want
            );
        }
        assert_eq!(
            as_string(&PvField::Scalar(ScalarValue::Float(0.1))).unwrap(),
            "0.1"
        );
    }

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
    fn as_u64_casts_every_scalar_storage_class() {
        // `copyOutScalar` → `uint64_t(src)` for bool / signed / unsigned /
        // real. Signed WRAPS; real truncates toward zero.
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Boolean(true))), Ok(1));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Int(7))), Ok(7));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Int(-1))), Ok(u64::MAX));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::ULong(9))), Ok(9));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Double(8.0))), Ok(8));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Double(8.9))), Ok(8));
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Float(3.5))), Ok(3));
        // Documented deviation: C++ UB for a negative / NaN real; the port
        // takes Rust's defined saturating cast.
        assert_eq!(as_u64(&PvField::Scalar(ScalarValue::Double(-1.0))), Ok(0));
        assert_eq!(
            as_u64(&PvField::Scalar(ScalarValue::Double(f64::NAN))),
            Ok(0)
        );
        // No arm for array / struct storage.
        assert!(
            as_u64(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
                vec![4].into()
            )))
            .is_err()
        );
        assert!(as_u64(&PvField::Structure(PvStructure::new(""))).is_err());
    }

    #[test]
    fn as_u32_truncates_like_store_transform() {
        // `StoreTransform<uint32_t>::out` binds the uint64 store to a
        // `const uint32_t&` — a truncation, not a range check.
        assert_eq!(
            as_u32(&PvField::Scalar(ScalarValue::Int(-1))),
            Ok(0xFFFF_FFFF)
        );
        assert_eq!(
            as_u32(&PvField::Scalar(ScalarValue::ULong(0x1_0000_0005))),
            Ok(5)
        );
        assert_eq!(as_u8(&PvField::Scalar(ScalarValue::Int(0x1_05))), Ok(5));
    }

    /// `std::stoull` defers to `strtoull`, whose leading skip is `isspace` —
    /// vertical tab included. Rust's `is_ascii_whitespace` omits it, which
    /// turned a value pvxs converts into `NoConvert`.
    #[test]
    fn a_leading_vertical_tab_is_whitespace_as_it_is_to_strtoull() {
        assert_eq!(parse_to_u64("\u{0b}16"), Ok(16));
        assert_eq!(parse_to_u64("16\u{0b}"), Ok(16));
    }

    #[test]
    fn parse_to_u64_is_stoull_base_zero() {
        // Radix from the prefix, exactly as `strtoull(s, &end, 0)` picks it.
        assert_eq!(parse_to_u64("16"), Ok(16));
        assert_eq!(parse_to_u64("0x10"), Ok(16));
        assert_eq!(parse_to_u64("0X10"), Ok(16));
        assert_eq!(parse_to_u64("010"), Ok(8));
        assert_eq!(parse_to_u64("0"), Ok(0));
        // A leading '-' negates the unsigned value (C, not an error).
        assert_eq!(parse_to_u64("-1"), Ok(u64::MAX));
        assert_eq!(parse_to_u64("+7"), Ok(7));
        // Leading and trailing whitespace are skipped; anything else is not.
        assert_eq!(parse_to_u64("  12  "), Ok(12));
        for bad in ["", "abc", "12abc", "1.5", "0x", "8", "9"].iter().take(5) {
            assert!(parse_to_u64(bad).is_err(), "{bad:?} must be NoConvert");
        }
        // Octal radix rejects 8/9 as digits — they become extraneous.
        assert!(parse_to_u64("08").is_err());
        // `0x` with no hex digit after it is no prefix: stoull takes the `0`.
        for s in ["0x", "0xg", "-0x", " 0X "] {
            assert_eq!(
                parse_to_u64(s).unwrap_err().message(),
                format!("Extraneous characters after integer: \"{s}\"")
            );
        }
        assert_eq!(parse_to_u64("0x1"), Ok(1));
        // Overflow is `std::out_of_range` → NoConvert.
        assert!(parse_to_u64("18446744073709551616").is_err());
    }

    /// libstdc++ `stoll`/`stoull` are glibc `strtoll`/`strtoull`, which g++'s
    /// `_GNU_SOURCE` builds with the C23 `0b` prefix under base 0.
    #[test]
    fn parse_to_takes_the_glibc_binary_prefix() {
        assert_eq!(parse_to_i64("0b101"), Ok(5));
        assert_eq!(parse_to_i64("-0b11"), Ok(-3));
        assert_eq!(parse_to_u64("0B11"), Ok(3));
        assert_eq!(
            parse_to_u64("0b2").unwrap_err().message(),
            "Extraneous characters after integer: \"0b2\""
        );
    }

    #[test]
    fn parse_to_i64_is_stoll_base_zero() {
        assert_eq!(parse_to_i64("-1"), Ok(-1));
        assert_eq!(parse_to_i64("0x10"), Ok(16));
        assert_eq!(parse_to_i64("-010"), Ok(-8));
        assert_eq!(parse_to_i64("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(
            parse_to_i64("9223372036854775808").unwrap_err().message(),
            "Out of range : \"9223372036854775808\""
        );
        // stoll raises out_of_range before parseTo looks at the tail.
        assert_eq!(
            parse_to_i64("9999999999999999999x").unwrap_err().message(),
            "Out of range : \"9999999999999999999x\""
        );
        assert_eq!(
            parse_to_i64("5x").unwrap_err().message(),
            "Extraneous characters after unsigned: \"5x\""
        );
        assert_eq!(
            parse_to_i64("x").unwrap_err().message(),
            "Invalid input : \"x\""
        );
    }

    #[test]
    fn as_u64_parses_a_string_store_base_zero() {
        // pvxs `copyOut` String→UInteger goes through `parseTo<uint64_t>`
        // (data.cpp:451-453), so a hex / octal / negative option STRING
        // converts.
        assert_eq!(
            as_u64(&PvField::Scalar(ScalarValue::String("0x10".into()))),
            Ok(16)
        );
        assert_eq!(
            as_u64(&PvField::Scalar(ScalarValue::String("010".into()))),
            Ok(8)
        );
        assert_eq!(
            as_u32(&PvField::Scalar(ScalarValue::String("-1".into()))),
            Ok(0xFFFF_FFFF)
        );
        assert!(as_u64(&PvField::Scalar(ScalarValue::String("50%".into()))).is_err());
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
