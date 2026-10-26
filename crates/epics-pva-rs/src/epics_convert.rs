//! `EpicsValue` → pvData as the qsrv bridge serves it and a PVA link puts
//! it. Here rather than in `epics-bridge-rs::convert`, which re-exports
//! both, because [`crate::pvalink`] puts through it.

use epics_base_rs::types::EpicsValue;

use crate::pvdata::{PvArray, PvField, ScalarValue, TypedScalarArray};

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

/// Convert EpicsValue to PvField (scalar or array).
///
/// An array becomes a [`PvField::ScalarArrayTyped`] reading the
/// `EpicsValue`'s own buffer: the record, its snapshot and the wire
/// encoder share one `SharedArray`, the way pvxs hands a monitor the
/// record's `shared_array` (`iocsource.cpp` `getValue`). Only `CharArray`
/// copies, since u8 storage cannot alias the signed `byte[]` it serves.
pub fn epics_to_pv_field(val: &EpicsValue) -> PvField {
    let typed = match val {
        EpicsValue::ShortArray(a) => TypedScalarArray::Short(PvArray::from(a)),
        EpicsValue::FloatArray(a) => TypedScalarArray::Float(PvArray::from(a)),
        EpicsValue::EnumArray(a) => TypedScalarArray::UShort(PvArray::from(a)),
        EpicsValue::DoubleArray(a) => TypedScalarArray::Double(PvArray::from(a)),
        EpicsValue::LongArray(a) => TypedScalarArray::Int(PvArray::from(a)),
        // DBF_CHAR ↔ pvByte (signed): the storage byte reinterpreted.
        EpicsValue::CharArray(a) => TypedScalarArray::Byte(a.iter().map(|&v| v as i8).collect()),
        EpicsValue::StringArray(a) => TypedScalarArray::String(PvArray::from(a)),
        EpicsValue::Int64Array(a) => TypedScalarArray::Long(PvArray::from(a)),
        EpicsValue::UInt64Array(a) => TypedScalarArray::ULong(PvArray::from(a)),
        // DBF_USHORT[]/DBF_ULONG[] serve as PVA ushort[]/uint[]. These MUST be
        // explicit: the `other =>` scalar fallback below would collapse the
        // array to its first element (pvxs `ioc/typeutils.cpp:38-44`).
        EpicsValue::UShortArray(a) => TypedScalarArray::UShort(PvArray::from(a)),
        EpicsValue::ULongArray(a) => TypedScalarArray::UInt(PvArray::from(a)),
        // DBF_UCHAR[] serves as PVA ubyte[] (unsigned), unlike CHAR[]'s signed
        // byte[] above (pvxs `ioc/typeutils.cpp:34-35` DBR_UCHAR→UInt8). MUST
        // be explicit: the `other =>` scalar fallback would collapse it.
        EpicsValue::UCharArray(a) => TypedScalarArray::UByte(PvArray::from(a)),
        other => return PvField::Scalar(epics_to_scalar(other)),
    };
    PvField::ScalarArrayTyped(typed)
}
