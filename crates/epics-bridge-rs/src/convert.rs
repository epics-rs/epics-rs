use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_pva_rs::pvdata::{PvField, ScalarType, ScalarValue};

/// Convert EPICS DBF type to PVA ScalarType.
///
/// Note: `DBF_CHAR` maps to `pvByte` (signed i8) per C qsrv, not `pvUByte`.
/// libca commit 7cb80d5a1 made `epicsInt8` signed; the PVA mapping follows
/// suit so a negative DBF_CHAR value round-trips with sign intact.
pub fn dbf_to_scalar_type(dbf: DbFieldType) -> ScalarType {
    match dbf {
        DbFieldType::String => ScalarType::String,
        DbFieldType::Short => ScalarType::Short,
        DbFieldType::Float => ScalarType::Float,
        DbFieldType::Enum => ScalarType::UShort, // C++ maps DBR_ENUM to pvUShort
        DbFieldType::Char => ScalarType::Byte,
        DbFieldType::Long => ScalarType::Int,
        DbFieldType::Double => ScalarType::Double,
        DbFieldType::Int64 => ScalarType::Long,
        // C `DBF_UINT64` → PVA `ulong` (native unsigned 64-bit).
        DbFieldType::UInt64 => ScalarType::ULong,
        // C `DBF_USHORT` → PVA `ushort` / `DBF_ULONG` → PVA `uint`
        // (pvxs `ioc/typeutils.cpp:38-44`).
        DbFieldType::UShort => ScalarType::UShort,
        DbFieldType::ULong => ScalarType::UInt,
        // C `DBF_UCHAR` → PVA `ubyte` (UInt8), the unsigned twin of the
        // signed `Char → Byte` above (pvxs `ioc/typeutils.cpp:34-35`).
        DbFieldType::UChar => ScalarType::UByte,
    }
}

/// Convert EpicsValue to PVA ScalarValue.
pub fn epics_to_scalar(val: &EpicsValue) -> ScalarValue {
    match val {
        EpicsValue::String(s) => ScalarValue::String(s.clone()),
        EpicsValue::Short(v) => ScalarValue::Short(*v),
        EpicsValue::Float(v) => ScalarValue::Float(*v),
        EpicsValue::Enum(v) => ScalarValue::UShort(*v), // C++: pvUShort
        // Transient NTEnum carrier never reaches qsrv serving (coerced in
        // base at the link-write boundary); convert its index like a DBF_ENUM.
        EpicsValue::EnumWithChoices { index, .. } => ScalarValue::UShort(*index),
        // C qsrv: DBF_CHAR → pvByte (signed). Bit-preserving cast keeps
        // the on-the-wire byte identical; only the typed interpretation
        // changes from unsigned to signed.
        EpicsValue::Char(v) => ScalarValue::Byte(*v as i8),
        EpicsValue::Long(v) => ScalarValue::Int(*v),
        EpicsValue::Double(v) => ScalarValue::Double(*v),
        EpicsValue::Int64(v) => ScalarValue::Long(*v),
        EpicsValue::UInt64(v) => ScalarValue::ULong(*v),
        // C `DBF_USHORT` → PVA `ushort` / `DBF_ULONG` → PVA `uint`
        // (pvxs `ioc/typeutils.cpp:38-44`).
        EpicsValue::UShort(v) => ScalarValue::UShort(*v),
        EpicsValue::ULong(v) => ScalarValue::UInt(*v),
        // C `DBF_UCHAR` → PVA `ubyte`, the unsigned twin of `Char → Byte`.
        EpicsValue::UChar(v) => ScalarValue::UByte(*v),
        // Arrays: take first element or default
        EpicsValue::ShortArray(a) => ScalarValue::Short(a.first().copied().unwrap_or(0)),
        EpicsValue::FloatArray(a) => ScalarValue::Float(a.first().copied().unwrap_or(0.0)),
        EpicsValue::EnumArray(a) => ScalarValue::UShort(a.first().copied().unwrap_or(0)),
        EpicsValue::DoubleArray(a) => ScalarValue::Double(a.first().copied().unwrap_or(0.0)),
        EpicsValue::LongArray(a) => ScalarValue::Int(a.first().copied().unwrap_or(0)),
        EpicsValue::CharArray(a) => ScalarValue::Byte(a.first().copied().unwrap_or(0) as i8),
        EpicsValue::StringArray(a) => ScalarValue::String(a.first().cloned().unwrap_or_default()),
        EpicsValue::Int64Array(a) => ScalarValue::Long(a.first().copied().unwrap_or(0)),
        EpicsValue::UInt64Array(a) => ScalarValue::ULong(a.first().copied().unwrap_or(0)),
        EpicsValue::UShortArray(a) => ScalarValue::UShort(a.first().copied().unwrap_or(0)),
        EpicsValue::ULongArray(a) => ScalarValue::UInt(a.first().copied().unwrap_or(0)),
        EpicsValue::UCharArray(a) => ScalarValue::UByte(a.first().copied().unwrap_or(0)),
    }
}

/// Convert PVA ScalarValue back to EpicsValue (context-free fallback).
///
/// Prefer `scalar_to_epics_typed()` when the target DBF type is known.
pub fn scalar_to_epics(val: &ScalarValue) -> EpicsValue {
    match val {
        ScalarValue::String(s) => EpicsValue::String(s.clone()),
        ScalarValue::Short(v) => EpicsValue::Short(*v),
        ScalarValue::Float(v) => EpicsValue::Float(*v),
        ScalarValue::Double(v) => EpicsValue::Double(*v),
        ScalarValue::Int(v) => EpicsValue::Long(*v),
        // `Long`/`ULong` are 64-bit; folding them into
        // `EpicsValue::Double` loses integer precision above the exact
        // `f64` integer range (2^53). `EpicsValue::Int64`/`UInt64`
        // exist now, so preserve the full 64-bit range — this is the
        // exact inverse of `epics_to_scalar` (`Int64 -> Long`,
        // `UInt64 -> ULong`) and matches the array path, which already
        // maps `Long[]`/`ULong[]` to `Int64Array`/`UInt64Array`.
        ScalarValue::Long(v) => EpicsValue::Int64(*v),
        // C qsrv: DBF_CHAR is signed (pvByte). Bit-preserving cast keeps
        // the storage byte identical; legacy UByte input still accepted
        // — we widen to Short to avoid clipping the unsigned 128..255 range.
        ScalarValue::Byte(v) => EpicsValue::Char(*v as u8),
        // PVA `ubyte` ↔ C `DBF_UCHAR` — the exact unsigned-8 carrier
        // `EpicsValue::UChar` now exists, so map it losslessly (0..255 fits)
        // and restore round-trip symmetry with `epics_to_scalar`'s
        // `UChar -> UByte`. The prior `Short` widening was a workaround for
        // the missing unsigned-8 variant (mirrors the `UInt -> Int64` note),
        // not a deliberate retype; it is now obsolete.
        ScalarValue::UByte(v) => EpicsValue::UChar(*v),
        ScalarValue::UShort(v) => EpicsValue::Enum(*v),
        // PVA `uint` (unsigned-32) has no unsigned-32 `EpicsValue`
        // variant. Carry the full range losslessly through `Int64` (a
        // `u32` always fits an `i64`), matching the documented C
        // `DBF_ULONG` convention in `filters/ts.rs` and symmetric with
        // the `ULong -> UInt64` / `Long -> Int64` arms. Folding it into
        // `Long(*v as i32)` sign-wrapped any value above `i32::MAX`
        // before the bound-field retype could observe its true
        // magnitude, so a `uint = 0x8000_0000` PUT became a negative
        // `Long` and could not be recovered for a wider unsigned target.
        ScalarValue::UInt(v) => EpicsValue::Int64(*v as i64),
        ScalarValue::ULong(v) => EpicsValue::UInt64(*v),
        ScalarValue::Boolean(v) => EpicsValue::Short(if *v { 1 } else { 0 }),
    }
}

/// A string scalar could not be parsed into the target numeric field.
///
/// Mirrors pvxs `parseTo<T>` (`util.cpp:769-817`), which throws
/// `NoConvert` on invalid input / out-of-range / trailing characters
/// rather than substituting a default. Returned by the typed PUT
/// conversion so the gateway/QSRV PUT path rejects the operation with a
/// client-visible error instead of silently writing 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarConvertError {
    /// The offending string value, as received from the client.
    pub value: String,
    /// The numeric kind the parse was attempted into (`f64`/`i64`/`u64`).
    pub target: &'static str,
}

impl std::fmt::Display for ScalarConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot convert string \"{}\" to {}",
            self.value, self.target
        )
    }
}

impl std::error::Error for ScalarConvertError {}

/// Context-aware conversion: PVA ScalarValue → EpicsValue using target DBF type.
///
/// Unlike `scalar_to_epics()`, this uses the target field type to produce the
/// correct EpicsValue variant, matching C++ PVIF behavior where conversions are
/// guided by `dbChannelFinalFieldType()`.
///
/// A string source bound for a numeric field is parsed strictly: an
/// unparseable / out-of-range / trailing-garbage string returns
/// [`ScalarConvertError`] so the caller rejects the PUT, matching pvxs
/// `parseTo<T>` (`util.cpp:769-817`) which throws `NoConvert` rather
/// than writing a default 0.
pub fn scalar_to_epics_typed(
    val: &ScalarValue,
    target: DbFieldType,
) -> Result<EpicsValue, ScalarConvertError> {
    Ok(match target {
        DbFieldType::Double => EpicsValue::Double(scalar_to_f64(val)?),
        DbFieldType::Float => EpicsValue::Float(scalar_to_f64(val)? as f32),
        DbFieldType::Long => EpicsValue::Long(scalar_to_i64(val)? as i32),
        DbFieldType::Int64 => EpicsValue::Int64(scalar_to_i64(val)?),
        DbFieldType::UInt64 => EpicsValue::UInt64(scalar_to_u64(val)?),
        DbFieldType::Short => EpicsValue::Short(scalar_to_i64(val)? as i16),
        DbFieldType::Char => EpicsValue::Char(scalar_to_i64(val)? as u8),
        DbFieldType::UChar => EpicsValue::UChar(scalar_to_i64(val)? as u8),
        DbFieldType::Enum => EpicsValue::Enum(scalar_to_i64(val)? as u16),
        // DBF_USHORT/DBF_ULONG narrow off the integer view (C static_cast
        // truncation, low 16/32 bits), mirroring the Short/Enum targets.
        DbFieldType::UShort => EpicsValue::UShort(scalar_to_i64(val)? as u16),
        DbFieldType::ULong => EpicsValue::ULong(scalar_to_i64(val)? as u32),
        DbFieldType::String => match val {
            ScalarValue::String(s) => EpicsValue::String(s.clone()),
            other => EpicsValue::String(other.to_string().into()),
        },
    })
}

/// Extract f64 from any ScalarValue.
///
/// A `String` source is parsed strictly (leading/trailing whitespace
/// tolerated to match `std::stod`); a non-numeric / out-of-range /
/// trailing-garbage string returns [`ScalarConvertError`] instead of
/// silently coercing to 0.0 — see pvxs `parseTo<double>`
/// (`util.cpp:769`).
fn scalar_to_f64(val: &ScalarValue) -> Result<f64, ScalarConvertError> {
    Ok(match val {
        ScalarValue::Double(v) => *v,
        ScalarValue::Float(v) => *v as f64,
        ScalarValue::Int(v) => *v as f64,
        ScalarValue::Long(v) => *v as f64,
        ScalarValue::Short(v) => *v as f64,
        ScalarValue::Byte(v) => *v as f64,
        ScalarValue::UByte(v) => *v as f64,
        ScalarValue::UShort(v) => *v as f64,
        ScalarValue::UInt(v) => *v as f64,
        ScalarValue::ULong(v) => *v as f64,
        ScalarValue::Boolean(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        ScalarValue::String(s) => {
            s.as_str_lossy()
                .trim()
                .parse()
                .map_err(|_| ScalarConvertError {
                    value: s.as_str_lossy().into_owned(),
                    target: "f64",
                })?
        }
    })
}

/// Extract i64 from any ScalarValue.
///
/// A `String` source is parsed strictly; a non-numeric / out-of-range /
/// trailing-garbage string returns [`ScalarConvertError`] instead of
/// silently coercing to 0 — see pvxs `parseTo<int64_t>`
/// (`util.cpp:803`).
fn scalar_to_i64(val: &ScalarValue) -> Result<i64, ScalarConvertError> {
    Ok(match val {
        ScalarValue::Int(v) => *v as i64,
        ScalarValue::Long(v) => *v,
        ScalarValue::Short(v) => *v as i64,
        ScalarValue::Byte(v) => *v as i64,
        ScalarValue::UByte(v) => *v as i64,
        ScalarValue::UShort(v) => *v as i64,
        ScalarValue::UInt(v) => *v as i64,
        ScalarValue::ULong(v) => *v as i64,
        ScalarValue::Double(v) => *v as i64,
        ScalarValue::Float(v) => *v as i64,
        ScalarValue::Boolean(v) => {
            if *v {
                1
            } else {
                0
            }
        }
        ScalarValue::String(s) => {
            // pvxs `parseTo<int64_t>` = `std::stoll(s,&idx,0)`: base-0 radix
            // (`0x` hex, leading-`0` octal). Reuse the CA path's C-radix
            // parser so the QSRV PUT accepts the same forms (and still
            // rejects empty/garbage).
            EpicsValue::parse_int(&s.as_str_lossy()).map_err(|_| ScalarConvertError {
                value: s.as_str_lossy().into_owned(),
                target: "i64",
            })?
        }
    })
}

/// Extract u64 from any ScalarValue, preserving the full unsigned range
/// when the source is itself an unsigned 64-bit value. Used for
/// `DBF_UINT64` PUT conversion — routing through `scalar_to_i64` would
/// reject `ulong` values above `i64::MAX`.
fn scalar_to_u64(val: &ScalarValue) -> Result<u64, ScalarConvertError> {
    Ok(match val {
        ScalarValue::ULong(v) => *v,
        ScalarValue::Long(v) => *v as u64,
        ScalarValue::Int(v) => *v as u64,
        ScalarValue::Short(v) => *v as u64,
        ScalarValue::Byte(v) => *v as u64,
        ScalarValue::UByte(v) => *v as u64,
        ScalarValue::UShort(v) => *v as u64,
        ScalarValue::UInt(v) => *v as u64,
        ScalarValue::Double(v) => *v as u64,
        ScalarValue::Float(v) => *v as u64,
        ScalarValue::Boolean(v) => {
            if *v {
                1
            } else {
                0
            }
        }
        ScalarValue::String(s) => {
            // pvxs `parseTo<uint64_t>` = `std::stoull(s,&idx,0)`: base-0 radix.
            // Reuse the CA path's C-radix unsigned parser (keeps the full
            // 64-bit range and `strtoul` sign-wrap behavior).
            EpicsValue::parse_uint(&s.as_str_lossy()).map_err(|_| ScalarConvertError {
                value: s.as_str_lossy().into_owned(),
                target: "u64",
            })?
        }
    })
}

/// Resolve an enum string to its index using a list of choice strings.
///
/// Corresponds to C++ dbf_copy.cpp enum string → index reverse lookup.
/// Returns None if the string doesn't match any choice.
pub fn enum_string_to_index(choices: &[String], name: &str) -> Option<u16> {
    choices.iter().position(|s| s == name).map(|i| i as u16)
}

/// Convert an enum index to its string representation.
pub fn enum_index_to_string(choices: &[String], index: u16) -> String {
    choices
        .get(index as usize)
        .cloned()
        .unwrap_or_else(|| format!("{index}"))
}

/// Convert EpicsValue to PvField (scalar or array).
pub fn epics_to_pv_field(val: &EpicsValue) -> PvField {
    match val {
        EpicsValue::ShortArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Short(*v)).collect())
        }
        EpicsValue::FloatArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Float(*v)).collect())
        }
        EpicsValue::EnumArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::UShort(*v)).collect())
        }
        EpicsValue::DoubleArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Double(*v)).collect())
        }
        EpicsValue::LongArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Int(*v)).collect())
        }
        EpicsValue::CharArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Byte(*v as i8)).collect())
        }
        EpicsValue::StringArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::String(v.clone())).collect())
        }
        EpicsValue::Int64Array(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::Long(*v)).collect())
        }
        EpicsValue::UInt64Array(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::ULong(*v)).collect())
        }
        // DBF_USHORT[]/DBF_ULONG[] serve as PVA ushort[]/uint[]. These MUST be
        // explicit: the `other =>` scalar fallback below would collapse the
        // array to its first element (pvxs `ioc/typeutils.cpp:38-44`).
        EpicsValue::UShortArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::UShort(*v)).collect())
        }
        EpicsValue::ULongArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::UInt(*v)).collect())
        }
        // DBF_UCHAR[] serves as PVA ubyte[] (unsigned), unlike CHAR[]'s signed
        // byte[] above (pvxs `ioc/typeutils.cpp:34-35` DBR_UCHAR→UInt8). MUST
        // be explicit: the `other =>` scalar fallback would collapse it.
        EpicsValue::UCharArray(a) => {
            PvField::ScalarArray(a.iter().map(|v| ScalarValue::UByte(*v)).collect())
        }
        other => PvField::Scalar(epics_to_scalar(other)),
    }
}

/// Empty-array `EpicsValue` carrier for a declared PVA element type.
///
/// Mirrors the non-empty `PvField::ScalarArray` element→variant dispatch
/// in [`pv_field_to_epics`] exactly, so an empty and a populated array of
/// the same wire type always select the same `EpicsValue` variant — no
/// type drift at the zero-length boundary. `Long`/`Boolean` have no
/// dedicated empty carrier here because the non-empty path routes them
/// through its `_ => DoubleArray` arm; matching that keeps the boundary
/// uniform (promoting `long[]` to `Int64Array` is a separate concern in
/// the non-empty arms, not this empty-type-preservation fix).
fn empty_typed_array(ty: ScalarType) -> EpicsValue {
    match ty {
        ScalarType::Double => EpicsValue::DoubleArray(vec![]),
        ScalarType::Float => EpicsValue::FloatArray(vec![]),
        ScalarType::Short => EpicsValue::ShortArray(vec![]),
        ScalarType::Int => EpicsValue::LongArray(vec![]),
        ScalarType::Byte => EpicsValue::CharArray(vec![]),
        ScalarType::UByte => EpicsValue::UCharArray(vec![]),
        ScalarType::UShort => EpicsValue::EnumArray(vec![]),
        ScalarType::String => EpicsValue::StringArray(vec![]),
        ScalarType::UInt => EpicsValue::Int64Array(vec![]),
        ScalarType::ULong => EpicsValue::UInt64Array(vec![]),
        ScalarType::Long | ScalarType::Boolean => EpicsValue::DoubleArray(vec![]),
    }
}

/// Extract EpicsValue from a PvField.
pub fn pv_field_to_epics(field: &PvField) -> Option<EpicsValue> {
    // Typed scalar arrays carry an authoritative element type tag. An
    // empty typed array has no element to infer from, so it MUST take
    // its variant from the tag — routing it through the untyped
    // first-element/empty path below would collapse it to `double[]`
    // and a typed empty PUT would silently retype a string/int/float
    // waveform carrier. pvxs keeps the descriptor element type at zero
    // count and selects the DBR type from the array's `original_type()`
    // (dataencode.cpp:315-352, iocsource.cpp:538-568). A non-empty typed
    // array still flows through the legacy per-element dispatch, which
    // agrees element-for-element with `empty_typed_array`.
    if let PvField::ScalarArrayTyped(arr) = field {
        if arr.is_empty() {
            return Some(empty_typed_array(arr.scalar_type()));
        }
        let legacy = PvField::ScalarArray(arr.to_scalar_values());
        return pv_field_to_epics(&legacy);
    }
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::ScalarArray(arr) => {
            if arr.is_empty() {
                // Genuinely untyped empty array (legacy `ScalarArray`
                // with no type tag): there is no element type to recover,
                // so `double[]` is the only available carrier. Typed
                // empty arrays never reach here — they are resolved from
                // their tag by `empty_typed_array` above.
                return Some(EpicsValue::DoubleArray(vec![]));
            }
            // Numeric arms dispatch on the homogeneous element type, so a
            // `String` element never reaches `scalar_to_{f64,i64,u64}`
            // here — but should a non-numeric string slip into a
            // numeric-typed array, `.ok()?` rejects the whole conversion
            // (→ `None`, surfaced as a PUT error by the caller) rather
            // than silently substituting 0, matching pvxs `parseTo<T>`.
            match &arr[0] {
                ScalarValue::Double(_) => Some(EpicsValue::DoubleArray(
                    arr.iter()
                        .map(scalar_to_f64)
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                ScalarValue::Float(_) => Some(EpicsValue::FloatArray(
                    arr.iter()
                        .map(|v| scalar_to_f64(v).map(|n| n as f32))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                ScalarValue::Short(_) => Some(EpicsValue::ShortArray(
                    arr.iter()
                        .map(|v| scalar_to_i64(v).map(|n| n as i16))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                ScalarValue::Int(_) => Some(EpicsValue::LongArray(
                    arr.iter()
                        .map(|v| scalar_to_i64(v).map(|n| n as i32))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                // Canonical: DBF_CHAR ↔ pvByte (signed).
                ScalarValue::Byte(_) => Some(EpicsValue::CharArray(
                    arr.iter()
                        .map(|v| scalar_to_i64(v).map(|n| n as u8))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                // pvUByte[] ↔ C `DBF_UCHAR[]` — the exact unsigned-8 carrier
                // `EpicsValue::UCharArray` now exists, so map it losslessly
                // and symmetric with `epics_to_pv_field`'s `UCharArray ->
                // UByte`. The prior widen-to-Short was a workaround for the
                // missing unsigned-8 variant; never fold into the signed
                // DBF_CHAR (`CharArray`) mapping.
                ScalarValue::UByte(_) => Some(EpicsValue::UCharArray(
                    arr.iter()
                        .map(|v| scalar_to_i64(v).map(|n| n as u8))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                ScalarValue::UShort(_) => Some(EpicsValue::EnumArray(
                    arr.iter()
                        .map(|v| scalar_to_i64(v).map(|n| n as u16))
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                ScalarValue::String(_) => Some(EpicsValue::StringArray(
                    arr.iter()
                        .map(|v| match v {
                            ScalarValue::String(s) => s.clone(),
                            other => other.to_string().into(),
                        })
                        .collect(),
                )),
                // PVA `uint[]` ↔ C `DBF_ULONG[]` — preserve the full
                // unsigned-32 range through `Int64Array` (no unsigned-32
                // `EpicsValue` variant exists), matching the `ts.rs`
                // `DBF_ULONG` convention and the scalar `UInt` arm. The
                // prior fallthrough folded `uint[]` into `DoubleArray`,
                // losing the unsigned-32 element type.
                ScalarValue::UInt(_) => Some(EpicsValue::Int64Array(
                    arr.iter()
                        .map(scalar_to_i64)
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                // PVA `ulong[]` ↔ C `DBF_UINT64[]` — preserve the full
                // unsigned range instead of folding into DoubleArray.
                ScalarValue::ULong(_) => Some(EpicsValue::UInt64Array(
                    arr.iter()
                        .map(scalar_to_u64)
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
                _ => Some(EpicsValue::DoubleArray(
                    arr.iter()
                        .map(scalar_to_f64)
                        .collect::<Result<_, _>>()
                        .ok()?,
                )),
            }
        }
        // Composite/union/variant values aren't directly representable as
        // EpicsValue in the qsrv→record direction; only scalar/scalar-array
        // fields flow back into the database.
        PvField::Structure(_)
        | PvField::StructureArray(_)
        | PvField::Union { .. }
        | PvField::UnionArray(_)
        | PvField::Variant(_)
        | PvField::VariantArray(_)
        | PvField::Null => None,
        // Handled at the top of the function — unreachable here.
        PvField::ScalarArrayTyped(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn br_r13_uint64_field_maps_to_pva_ulong() {
        // a C `DBF_UINT64` field must map to PVA `ulong`, and a
        // value above `i64::MAX` must survive conversion in both
        // directions. On main `DbFieldType::UInt64` / `EpicsValue::UInt64`
        // did not exist, so unsigned-64 fields could not be represented.
        let big: u64 = u64::MAX - 7; // well above i64::MAX

        // DBF type → PVA scalar type
        assert_eq!(dbf_to_scalar_type(DbFieldType::UInt64), ScalarType::ULong);

        // EpicsValue::UInt64 → ScalarValue::ULong, full range preserved
        let sv = epics_to_scalar(&EpicsValue::UInt64(big));
        assert_eq!(sv, ScalarValue::ULong(big));

        // PUT path: ScalarValue::ULong → EpicsValue::UInt64 (typed)
        let ev = scalar_to_epics_typed(&ScalarValue::ULong(big), DbFieldType::UInt64).unwrap();
        assert_eq!(ev, EpicsValue::UInt64(big));

        // Array path: UInt64Array ↔ ulong[] round-trip, full range
        let arr = EpicsValue::UInt64Array(vec![0, big, i64::MAX as u64 + 1]);
        let pf = epics_to_pv_field(&arr);
        match &pf {
            PvField::ScalarArray(vs) => {
                assert!(matches!(vs[1], ScalarValue::ULong(v) if v == big));
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }
        let back = pv_field_to_epics(&pf).unwrap();
        assert_eq!(back, arr);
    }

    /// A PVA `uint` above `i32::MAX` must survive context-free
    /// extraction with its magnitude intact. The lossy
    /// `UInt(v) => Long(v as i32)` arm sign-wrapped `0x8000_0000` to
    /// `-2147483648`, so a later retype into a wider unsigned target
    /// recovered a completely different number. `Int64` carries the full
    /// unsigned-32 range (a `u32` always fits an `i64`).
    #[test]
    fn uint_scalar_preserves_full_unsigned32_range() {
        let v: u32 = 0x8000_0000; // above i32::MAX
        let ev = scalar_to_epics(&ScalarValue::UInt(v));
        assert_eq!(ev, EpicsValue::Int64(2_147_483_648));

        // The single-record PUT chain: extracted Int64 -> ScalarValue::Long
        // -> typed retype into a DBF_UINT64 target preserves the value,
        // unlike the old path that produced 18446744071562067968.
        let sv = epics_to_scalar(&ev);
        assert_eq!(sv, ScalarValue::Long(2_147_483_648));
        let typed = scalar_to_epics_typed(&sv, DbFieldType::UInt64).unwrap();
        assert_eq!(typed, EpicsValue::UInt64(2_147_483_648));
    }

    /// A PVA `uint[]` must round-trip through `Int64Array` (the
    /// `DBF_ULONG[]` carrier), not collapse into `DoubleArray`. The prior
    /// missing arm folded the array into `DoubleArray`, losing the
    /// unsigned-32 element type and, for values above `2^53`, precision.
    #[test]
    fn uint_array_preserves_element_type_and_range() {
        let big: u32 = 0xFFFF_FFFF;
        let pf = PvField::ScalarArray(vec![
            ScalarValue::UInt(0),
            ScalarValue::UInt(0x8000_0000),
            ScalarValue::UInt(big),
        ]);
        let ev = pv_field_to_epics(&pf).unwrap();
        assert_eq!(
            ev,
            EpicsValue::Int64Array(vec![0, 2_147_483_648, 4_294_967_295])
        );
    }

    #[test]
    fn roundtrip_double() {
        let orig = EpicsValue::Double(2.5);
        let sv = epics_to_scalar(&orig);
        let back = scalar_to_epics(&sv);
        assert_eq!(orig, back);
    }

    #[test]
    fn roundtrip_string() {
        let orig = EpicsValue::String("hello".into());
        let sv = epics_to_scalar(&orig);
        let back = scalar_to_epics(&sv);
        assert_eq!(orig, back);
    }

    /// PVA-89: the gateway string-value pass-through
    /// (`epics_to_scalar` ⇄ `scalar_to_epics`) clones the `PvString`, so a
    /// non-UTF-8 / Latin-1 byte run survives the EPICS⇆PVA round-trip
    /// verbatim — pvxs stores wire strings as raw bytes with no UTF-8
    /// validation (`pvaproto.h:403`), and the gateway must not mangle them.
    #[test]
    fn roundtrip_string_non_utf8_bytes() {
        use epics_base_rs::types::PvString;
        let raw = vec![0xff, 0x80, b'a', 0xc3, 0x28];
        let orig = EpicsValue::String(PvString::from_bytes(raw.clone()));
        let sv = epics_to_scalar(&orig);
        match &sv {
            ScalarValue::String(s) => assert_eq!(
                s.as_bytes(),
                raw.as_slice(),
                "EPICS→PVA leg must preserve raw bytes"
            ),
            other => panic!("expected ScalarValue::String, got {other:?}"),
        }
        let back = scalar_to_epics(&sv);
        match &back {
            EpicsValue::String(s) => assert_eq!(
                s.as_bytes(),
                raw.as_slice(),
                "PVA→EPICS leg must preserve raw bytes"
            ),
            other => panic!("expected EpicsValue::String, got {other:?}"),
        }
        assert_eq!(orig, back, "full gateway round-trip is byte-lossless");
    }

    #[test]
    fn roundtrip_short() {
        let orig = EpicsValue::Short(42);
        let sv = epics_to_scalar(&orig);
        let back = scalar_to_epics(&sv);
        assert_eq!(orig, back);
    }

    #[test]
    fn roundtrip_enum() {
        let orig = EpicsValue::Enum(3);
        let sv = epics_to_scalar(&orig);
        assert!(matches!(sv, ScalarValue::UShort(3)));
        let back = scalar_to_epics(&sv);
        assert_eq!(orig, back);
    }

    #[test]
    fn double_array_roundtrip() {
        let orig = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]);
        let pf = epics_to_pv_field(&orig);
        let back = pv_field_to_epics(&pf).unwrap();
        assert_eq!(orig, back);
    }

    #[test]
    fn dbf_type_mapping() {
        assert_eq!(dbf_to_scalar_type(DbFieldType::Double), ScalarType::Double);
        assert_eq!(dbf_to_scalar_type(DbFieldType::String), ScalarType::String);
        assert_eq!(dbf_to_scalar_type(DbFieldType::Short), ScalarType::Short);
        assert_eq!(dbf_to_scalar_type(DbFieldType::Long), ScalarType::Int);
        assert_eq!(dbf_to_scalar_type(DbFieldType::Char), ScalarType::Byte);
        assert_eq!(dbf_to_scalar_type(DbFieldType::Enum), ScalarType::UShort);
    }

    #[test]
    fn typed_conversion_double() {
        let sv = ScalarValue::Int(42);
        let ev = scalar_to_epics_typed(&sv, DbFieldType::Double).unwrap();
        assert_eq!(ev, EpicsValue::Double(42.0));
    }

    #[test]
    fn typed_conversion_enum() {
        let sv = ScalarValue::Int(5);
        let ev = scalar_to_epics_typed(&sv, DbFieldType::Enum).unwrap();
        assert_eq!(ev, EpicsValue::Enum(5));
    }

    #[test]
    fn typed_conversion_string_from_numeric() {
        let sv = ScalarValue::Double(2.5);
        let ev = scalar_to_epics_typed(&sv, DbFieldType::String).unwrap();
        assert!(matches!(ev, EpicsValue::String(_)));
    }

    /// A numeric string PUT into a numeric field parses to the value
    /// (pvxs `parseTo<T>` accepts it); surrounding whitespace is
    /// tolerated to match `std::stod`/`stoull`.
    #[test]
    fn string_to_numeric_put_parses_valid() {
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("42".into()), DbFieldType::Double).unwrap(),
            EpicsValue::Double(42.0)
        );
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("  -7  ".into()), DbFieldType::Long)
                .unwrap(),
            EpicsValue::Long(-7)
        );
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("255".into()), DbFieldType::UInt64).unwrap(),
            EpicsValue::UInt64(255)
        );
    }

    /// pvxs `parseTo<int64_t/uint64_t>` use `stoll/stoull(s,&idx,0)` — base-0,
    /// so a `0x` hex or leading-`0` octal string PUT converts (`pvput PV
    /// "0x1F"` → 31). The bridge previously parsed base-10 only and rejected
    /// these, while the CA sibling already did C-radix.
    #[test]
    fn string_to_numeric_put_accepts_c_radix() {
        // Hex into a signed width (Long routes through scalar_to_i64).
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("0x1F".into()), DbFieldType::Long).unwrap(),
            EpicsValue::Long(31)
        );
        // Leading-zero octal.
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("017".into()), DbFieldType::Int64).unwrap(),
            EpicsValue::Int64(15)
        );
        // Hex into the full-range unsigned path (UInt64 → scalar_to_u64).
        assert_eq!(
            scalar_to_epics_typed(&ScalarValue::String("0xFF".into()), DbFieldType::UInt64)
                .unwrap(),
            EpicsValue::UInt64(255)
        );
    }

    /// An unparseable / trailing-garbage / out-of-range string PUT into a
    /// numeric field must be rejected (`ScalarConvertError`), matching
    /// pvxs `parseTo<T>` which throws `NoConvert` — NOT silently written
    /// as 0. Before the fix `scalar_to_*` did `parse().unwrap_or(0)`, so
    /// the gateway/QSRV PUT path wrote 0 and reported success.
    #[test]
    fn string_to_numeric_put_rejects_unconvertible() {
        // Non-numeric.
        assert!(
            scalar_to_epics_typed(&ScalarValue::String("abc".into()), DbFieldType::Double).is_err()
        );
        // Trailing garbage after a number.
        assert!(
            scalar_to_epics_typed(&ScalarValue::String("42abc".into()), DbFieldType::Long).is_err()
        );
        // Integer out-of-range (exceeds i64).
        assert!(
            scalar_to_epics_typed(
                &ScalarValue::String("99999999999999999999".into()),
                DbFieldType::Int64
            )
            .is_err()
        );
        // Empty string is not 0.
        assert!(
            scalar_to_epics_typed(&ScalarValue::String("".into()), DbFieldType::Short).is_err()
        );
    }

    #[test]
    fn f9_dbf_char_signed_roundtrip() {
        // DBF_CHAR maps to pvByte (signed). A negative value (-1 stored
        // as 0xFF) must serialize as ScalarValue::Byte(-1), then round-trip
        // back to the same byte pattern in EpicsValue::Char.
        let orig = EpicsValue::Char(0xFFu8); // bit pattern for -1 as i8
        let sv = epics_to_scalar(&orig);
        assert!(matches!(sv, ScalarValue::Byte(-1)));
        let back = scalar_to_epics(&sv);
        assert_eq!(back, EpicsValue::Char(0xFFu8));
    }

    #[test]
    fn f9_dbf_char_array_signed_roundtrip() {
        // Array path mirrors the scalar path: bit-preserving Byte mapping.
        let orig = EpicsValue::CharArray(vec![0u8, 1, 0xFE, 0xFF]); // 0,1,-2,-1 as i8
        let pf = epics_to_pv_field(&orig);
        if let PvField::ScalarArray(arr) = &pf {
            assert!(matches!(arr[0], ScalarValue::Byte(0)));
            assert!(matches!(arr[2], ScalarValue::Byte(-2)));
            assert!(matches!(arr[3], ScalarValue::Byte(-1)));
        } else {
            panic!("expected ScalarArray");
        }
        let back = pv_field_to_epics(&pf).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn q14_dbf_uchar_unsigned_roundtrip() {
        // DBF_UCHAR maps to pvUByte (unsigned), the unsigned twin of
        // DBF_CHAR->pvByte. The high value 0xFF must stay 255 (unsigned),
        // NOT wrap to -1 as the signed Char path does.
        let orig = EpicsValue::UChar(0xFFu8);
        let sv = epics_to_scalar(&orig);
        assert!(matches!(sv, ScalarValue::UByte(255)));
        let back = scalar_to_epics(&sv);
        assert_eq!(back, EpicsValue::UChar(0xFFu8));
    }

    #[test]
    fn q14_dbf_uchar_array_unsigned_roundtrip() {
        // FTVL=UCHAR waveform serves as PVA ubyte[] (unsigned). Element 200
        // stays 200, and the full array round-trips back to UCharArray —
        // never collapsing into the signed DBF_CHAR (Byte) space.
        let orig = EpicsValue::UCharArray(vec![0u8, 1, 200, 0xFF]);
        let pf = epics_to_pv_field(&orig);
        if let PvField::ScalarArray(arr) = &pf {
            assert!(matches!(arr[2], ScalarValue::UByte(200)));
            assert!(matches!(arr[3], ScalarValue::UByte(255)));
        } else {
            panic!("expected ScalarArray of UByte, got {pf:?}");
        }
        let back = pv_field_to_epics(&pf).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn q14_legacy_ubyte_maps_to_exact_uchar() {
        // Now that the exact unsigned-8 carrier EpicsValue::UChar exists,
        // an incoming pvUByte maps to it losslessly (restoring round-trip
        // symmetry with epics_to_scalar's UChar->UByte) instead of the
        // prior widen-to-Short workaround.
        let sv = ScalarValue::UByte(200);
        let ev = scalar_to_epics(&sv);
        assert_eq!(ev, EpicsValue::UChar(200));
    }

    /// a scalar PVA `ulong` extracted through the context-free
    /// fallback `scalar_to_epics` must preserve the full unsigned
    /// 64-bit range. The branch folded `ScalarValue::ULong` into
    /// `EpicsValue::Double(v as f64)`, losing integer precision above
    /// `2^53`. `EpicsValue::UInt64` exists now, so the conversion must
    /// keep it — symmetric with `epics_to_scalar` (`UInt64 -> ULong`)
    /// and with the array path (`ULong[] -> UInt64Array`).
    #[test]
    fn mr_r22_scalar_to_epics_preserves_ulong_precision() {
        // Above the exact-integer range of f64 (2^53).
        let big: u64 = u64::MAX - 7;
        assert!(big > (1u64 << 53), "test value must exceed f64 precision");

        let ev = scalar_to_epics(&ScalarValue::ULong(big));
        assert_eq!(
            ev,
            EpicsValue::UInt64(big),
            "ScalarValue::ULong must convert to EpicsValue::UInt64, not a \
             precision-lost Double"
        );

        // Signed 64-bit sibling: ScalarValue::Long must preserve i64
        // precision the same way (was also folded into Double).
        let big_i: i64 = i64::MAX - 3;
        let ev_i = scalar_to_epics(&ScalarValue::Long(big_i));
        assert_eq!(ev_i, EpicsValue::Int64(big_i));
    }

    /// An EMPTY typed scalar array (e.g. clearing a waveform) must keep
    /// its declared element type instead of collapsing to `double[]`.
    /// Before the fix every empty array — typed or not — was funneled
    /// through the untyped `ScalarArray` path and returned
    /// `EpicsValue::DoubleArray(vec![])`, silently retyping a string /
    /// int / float waveform carrier on an empty PUT. pvxs keeps the
    /// descriptor element type at zero count (dataencode.cpp:315-352)
    /// and selects the DBR type from `original_type()`
    /// (iocsource.cpp:538-568).
    #[test]
    fn empty_typed_array_preserves_element_type() {
        use epics_base_rs::types::PvString;
        use epics_pva_rs::pvdata::TypedScalarArray;
        use std::sync::Arc;

        let cases: &[(TypedScalarArray, EpicsValue)] = &[
            (
                TypedScalarArray::String(Arc::from([] as [PvString; 0])),
                EpicsValue::StringArray(vec![]),
            ),
            (
                TypedScalarArray::Int(Arc::from([] as [i32; 0])),
                EpicsValue::LongArray(vec![]),
            ),
            (
                TypedScalarArray::Float(Arc::from([] as [f32; 0])),
                EpicsValue::FloatArray(vec![]),
            ),
            (
                TypedScalarArray::Double(Arc::from([] as [f64; 0])),
                EpicsValue::DoubleArray(vec![]),
            ),
            (
                TypedScalarArray::Byte(Arc::from([] as [i8; 0])),
                EpicsValue::CharArray(vec![]),
            ),
            (
                TypedScalarArray::Short(Arc::from([] as [i16; 0])),
                EpicsValue::ShortArray(vec![]),
            ),
            (
                TypedScalarArray::UInt(Arc::from([] as [u32; 0])),
                EpicsValue::Int64Array(vec![]),
            ),
            (
                TypedScalarArray::ULong(Arc::from([] as [u64; 0])),
                EpicsValue::UInt64Array(vec![]),
            ),
        ];

        for (typed, expected) in cases {
            let got = pv_field_to_epics(&PvField::ScalarArrayTyped(typed.clone()))
                .expect("empty typed array converts");
            assert_eq!(
                &got,
                expected,
                "empty {:?} must preserve its element type, not become double[]",
                typed.scalar_type()
            );
        }
    }

    /// Boundary uniformity: an empty typed array and a one-element typed
    /// array of the SAME wire type must select the same `EpicsValue`
    /// variant, so the zero-length case introduces no type drift.
    #[test]
    fn empty_and_singleton_typed_arrays_share_variant() {
        use epics_pva_rs::pvdata::TypedScalarArray;
        use std::mem::discriminant;
        use std::sync::Arc;

        let empty = pv_field_to_epics(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
            Arc::from([] as [i32; 0]),
        )))
        .unwrap();
        let one = pv_field_to_epics(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
            Arc::from([7i32]),
        )))
        .unwrap();
        assert_eq!(
            discriminant(&empty),
            discriminant(&one),
            "empty int[] and [7] must both map to the same EpicsValue variant"
        );
    }
}
