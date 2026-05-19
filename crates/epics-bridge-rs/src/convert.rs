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
    }
}

/// Convert EpicsValue to PVA ScalarValue.
pub fn epics_to_scalar(val: &EpicsValue) -> ScalarValue {
    match val {
        EpicsValue::String(s) => ScalarValue::String(s.clone()),
        EpicsValue::Short(v) => ScalarValue::Short(*v),
        EpicsValue::Float(v) => ScalarValue::Float(*v),
        EpicsValue::Enum(v) => ScalarValue::UShort(*v), // C++: pvUShort
        // C qsrv: DBF_CHAR → pvByte (signed). Bit-preserving cast keeps
        // the on-the-wire byte identical; only the typed interpretation
        // changes from unsigned to signed.
        EpicsValue::Char(v) => ScalarValue::Byte(*v as i8),
        EpicsValue::Long(v) => ScalarValue::Int(*v),
        EpicsValue::Double(v) => ScalarValue::Double(*v),
        EpicsValue::Int64(v) => ScalarValue::Long(*v),
        EpicsValue::UInt64(v) => ScalarValue::ULong(*v),
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
        ScalarValue::Long(v) => EpicsValue::Double(*v as f64),
        // C qsrv: DBF_CHAR is signed (pvByte). Bit-preserving cast keeps
        // the storage byte identical; legacy UByte input still accepted
        // — we widen to Short to avoid clipping the unsigned 128..255 range.
        ScalarValue::Byte(v) => EpicsValue::Char(*v as u8),
        ScalarValue::UByte(v) => EpicsValue::Short(*v as i16),
        ScalarValue::UShort(v) => EpicsValue::Enum(*v),
        ScalarValue::UInt(v) => EpicsValue::Long(*v as i32),
        ScalarValue::ULong(v) => EpicsValue::Double(*v as f64),
        ScalarValue::Boolean(v) => EpicsValue::Short(if *v { 1 } else { 0 }),
    }
}

/// Context-aware conversion: PVA ScalarValue → EpicsValue using target DBF type.
///
/// Unlike `scalar_to_epics()`, this uses the target field type to produce the
/// correct EpicsValue variant, matching C++ PVIF behavior where conversions are
/// guided by `dbChannelFinalFieldType()`.
pub fn scalar_to_epics_typed(val: &ScalarValue, target: DbFieldType) -> EpicsValue {
    match target {
        DbFieldType::Double => EpicsValue::Double(scalar_to_f64(val)),
        DbFieldType::Float => EpicsValue::Float(scalar_to_f64(val) as f32),
        DbFieldType::Long => EpicsValue::Long(scalar_to_i64(val) as i32),
        DbFieldType::Int64 => EpicsValue::Int64(scalar_to_i64(val)),
        DbFieldType::UInt64 => EpicsValue::UInt64(scalar_to_u64(val)),
        DbFieldType::Short => EpicsValue::Short(scalar_to_i64(val) as i16),
        DbFieldType::Char => EpicsValue::Char(scalar_to_i64(val) as u8),
        DbFieldType::Enum => EpicsValue::Enum(scalar_to_i64(val) as u16),
        DbFieldType::String => match val {
            ScalarValue::String(s) => EpicsValue::String(s.clone()),
            other => EpicsValue::String(other.to_string()),
        },
    }
}

/// Extract f64 from any ScalarValue.
fn scalar_to_f64(val: &ScalarValue) -> f64 {
    match val {
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
        ScalarValue::String(s) => s.parse().unwrap_or(0.0),
    }
}

/// Extract i64 from any ScalarValue.
fn scalar_to_i64(val: &ScalarValue) -> i64 {
    match val {
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
        ScalarValue::String(s) => s.parse().unwrap_or(0),
    }
}

/// Extract u64 from any ScalarValue, preserving the full unsigned range
/// when the source is itself an unsigned 64-bit value. Used for
/// `DBF_UINT64` PUT conversion — routing through `scalar_to_i64` would
/// reject `ulong` values above `i64::MAX`.
fn scalar_to_u64(val: &ScalarValue) -> u64 {
    match val {
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
        ScalarValue::String(s) => s.parse().unwrap_or(0),
    }
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
        other => PvField::Scalar(epics_to_scalar(other)),
    }
}

/// Extract EpicsValue from a PvField.
pub fn pv_field_to_epics(field: &PvField) -> Option<EpicsValue> {
    // F-G11 transition: typed scalar arrays land here too. Convert
    // back through `to_scalar_values` so the existing per-type
    // dispatch keeps working without duplicating the logic.
    if let PvField::ScalarArrayTyped(arr) = field {
        let legacy = PvField::ScalarArray(arr.to_scalar_values());
        return pv_field_to_epics(&legacy);
    }
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::ScalarArray(arr) => {
            if arr.is_empty() {
                return Some(EpicsValue::DoubleArray(vec![]));
            }
            match &arr[0] {
                ScalarValue::Double(_) => Some(EpicsValue::DoubleArray(
                    arr.iter().map(scalar_to_f64).collect(),
                )),
                ScalarValue::Float(_) => Some(EpicsValue::FloatArray(
                    arr.iter().map(|v| scalar_to_f64(v) as f32).collect(),
                )),
                ScalarValue::Short(_) => Some(EpicsValue::ShortArray(
                    arr.iter().map(|v| scalar_to_i64(v) as i16).collect(),
                )),
                ScalarValue::Int(_) => Some(EpicsValue::LongArray(
                    arr.iter().map(|v| scalar_to_i64(v) as i32).collect(),
                )),
                // Canonical: DBF_CHAR ↔ pvByte (signed).
                ScalarValue::Byte(_) => Some(EpicsValue::CharArray(
                    arr.iter().map(|v| scalar_to_i64(v) as u8).collect(),
                )),
                // Legacy: pvUByte arrays widen to Short to preserve the
                // unsigned range; never silently fold into the new signed
                // DBF_CHAR mapping.
                ScalarValue::UByte(_) => Some(EpicsValue::ShortArray(
                    arr.iter().map(|v| scalar_to_i64(v) as i16).collect(),
                )),
                ScalarValue::UShort(_) => Some(EpicsValue::EnumArray(
                    arr.iter().map(|v| scalar_to_i64(v) as u16).collect(),
                )),
                ScalarValue::String(_) => Some(EpicsValue::StringArray(
                    arr.iter()
                        .map(|v| match v {
                            ScalarValue::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect(),
                )),
                // PVA `ulong[]` ↔ C `DBF_UINT64[]` — preserve the full
                // unsigned range instead of folding into DoubleArray.
                ScalarValue::ULong(_) => Some(EpicsValue::UInt64Array(
                    arr.iter().map(scalar_to_u64).collect(),
                )),
                _ => Some(EpicsValue::DoubleArray(
                    arr.iter().map(scalar_to_f64).collect(),
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
        // BR-R13: a C `DBF_UINT64` field must map to PVA `ulong`, and a
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
        let ev = scalar_to_epics_typed(&ScalarValue::ULong(big), DbFieldType::UInt64);
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
        let ev = scalar_to_epics_typed(&sv, DbFieldType::Double);
        assert_eq!(ev, EpicsValue::Double(42.0));
    }

    #[test]
    fn typed_conversion_enum() {
        let sv = ScalarValue::Int(5);
        let ev = scalar_to_epics_typed(&sv, DbFieldType::Enum);
        assert_eq!(ev, EpicsValue::Enum(5));
    }

    #[test]
    fn typed_conversion_string_from_numeric() {
        let sv = ScalarValue::Double(2.5);
        let ev = scalar_to_epics_typed(&sv, DbFieldType::String);
        assert!(matches!(ev, EpicsValue::String(_)));
    }

    #[test]
    fn f9_dbf_char_signed_roundtrip() {
        // F9: DBF_CHAR maps to pvByte (signed). A negative value (-1 stored
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
    fn f9_legacy_ubyte_widens_to_short() {
        // For backward compat: an incoming pvUByte (unsigned) widens to
        // Short rather than collapsing into the new signed DBF_CHAR space.
        let sv = ScalarValue::UByte(200);
        let ev = scalar_to_epics(&sv);
        assert_eq!(ev, EpicsValue::Short(200));
    }
}
