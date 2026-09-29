//! Single owner of the `EpicsValue` ↔ PVA value-leaf type correspondence
//! for the **serve and monitor-filter directions**.
//!
//! Three converters used to hand-transcribe this same mapping — the
//! single-record serve path (`server::native_source::snapshot_to_pv_field`
//! and `snapshot_to_field_desc`) and the monitor-filter bridge
//! (`server_native::tcp`'s forward + backward converters). That triplication
//! is what let a `DBF_CHAR`-signedness fix land on the serve path (de500251)
//! while the monitor-filter bridge kept the old unsigned mapping and turned
//! every filtered `DBF_CHAR` monitor into a `DescriptorMismatch` (f9812674).
//! Collapsing the mapping to one owner per direction makes that class of drift
//! unrepresentable: a signedness or type-code change is edited in exactly one
//! place and both call sites move together.
//!
//! `DBF_CHAR` is signed (`byte`, Int8) and `DBF_UCHAR` unsigned (`ubyte`,
//! UInt8) on the wire — pvxs `ioc/typeutils.cpp:32-35`
//! (`DBR_CHAR -> TypeCode::Int8`, `DBR_UCHAR -> TypeCode::UInt8`). The 32/64-bit
//! integer *names* differ between the two type systems (PVA `Int` is 32-bit =
//! `EpicsValue::Long`; PVA `Long` is 64-bit = `EpicsValue::Int64`); pinning the
//! correspondence here keeps those from drifting too.
//!
//! Scope boundary — this owner covers **only** the serve/monitor leaf
//! direction. It must NOT absorb the inbound PUT-decode path
//! (`native_source::scalar_to_epics` / `typed_array_to_epics`), whose
//! `UByte -> Char` single-carrier fold is a deliberately different rule: that
//! `EpicsValue` is a bit-preserving intermediate `put_pv` re-coerces to the
//! record's declared DBF type and which never reaches a served descriptor.
//! Forcing the distinct-carrier round-trip below onto that path would break
//! the re-coercion.

use crate::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue, TypedScalarArray};
use epics_base_rs::types::EpicsValue;
// Only the PVA → EpicsValue direction rebuilds a `PvString`.
use epics_base_rs::types::PvString;

/// `EpicsValue` → PVA value-leaf `PvField` (scalar or scalar array).
///
/// The single owner of the serve value leaf (`snapshot_to_pv_field`, after its
/// `DBR_ENUM` → NTEnum intercept) and the monitor-filter backward bridge
/// (`epics_value_to_pv_field`). Total over `EpicsValue`: adding a variant fails
/// to compile here, in one place, instead of silently diverging across the two
/// call sites.
pub(crate) fn epics_value_to_pv_leaf(v: &EpicsValue) -> PvField {
    match v {
        EpicsValue::Double(d) => PvField::Scalar(ScalarValue::Double(*d)),
        EpicsValue::Float(f) => PvField::Scalar(ScalarValue::Float(*f)),
        EpicsValue::Long(i) => PvField::Scalar(ScalarValue::Int(*i)),
        EpicsValue::Short(s) => PvField::Scalar(ScalarValue::Short(*s)),
        // C `DBF_CHAR` → PVA `byte` (Int8, signed), pvxs
        // `ioc/typeutils.cpp:32-33`. `Char` stores `u8`, so reinterpret with
        // `as i8` (wire byte unchanged); the unsigned `UChar → ubyte` twin
        // below stays correct. Serving it as `ubyte` reads 200 as 200 where the
        // client expects −56.
        EpicsValue::Char(c) => PvField::Scalar(ScalarValue::Byte(*c as i8)),
        EpicsValue::Enum(e) => PvField::Scalar(ScalarValue::Int(*e as i32)),
        // Transient NTEnum carrier never reaches a record snapshot / filter
        // result (coerced in base at the link-write boundary); serve its index
        // like a DBF_ENUM.
        EpicsValue::EnumWithChoices { index, .. } => {
            PvField::Scalar(ScalarValue::Int(*index as i32))
        }
        EpicsValue::String(s) => PvField::Scalar(ScalarValue::String(s.clone())),
        EpicsValue::Int64(l) => PvField::Scalar(ScalarValue::Long(*l)),
        // C `DBF_UINT64` → PVA `ulong` (native unsigned 64-bit).
        EpicsValue::UInt64(u) => PvField::Scalar(ScalarValue::ULong(*u)),
        // C `DBF_USHORT` → PVA `ushort` / `DBF_ULONG` → PVA `uint`
        // (pvxs `ioc/typeutils.cpp:38-44`: DBR_USHORT→UInt16, DBR_ULONG→UInt32).
        EpicsValue::UShort(u) => PvField::Scalar(ScalarValue::UShort(*u)),
        EpicsValue::ULong(u) => PvField::Scalar(ScalarValue::UInt(*u)),
        // C `DBF_UCHAR` → PVA `ubyte` (UInt8), pvxs `ioc/typeutils.cpp:34-35`.
        EpicsValue::UChar(c) => PvField::Scalar(ScalarValue::UByte(*c)),
        // Arrays are served as the typed wire form over the record's own
        // buffer (`PvArray::from(&SharedArray)` shares it); only the element
        // casts (`u8 -> i8`, `u16 -> i32`) copy.
        EpicsValue::DoubleArray(a) => typed(TypedScalarArray::Double(a.into())),
        EpicsValue::FloatArray(a) => typed(TypedScalarArray::Float(a.into())),
        EpicsValue::LongArray(a) => typed(TypedScalarArray::Int(a.into())),
        EpicsValue::ShortArray(a) => typed(TypedScalarArray::Short(a.into())),
        // C `DBF_CHAR[]` → PVA `byte[]` (Int8), pvxs `ioc/typeutils.cpp:32-33`
        // — the signed twin of `UCharArray → ubyte[]` below.
        EpicsValue::CharArray(a) => typed(TypedScalarArray::Byte(
            a.iter().map(|x| *x as i8).collect::<Vec<i8>>().into(),
        )),
        EpicsValue::EnumArray(a) => typed(TypedScalarArray::Int(
            a.iter().map(|x| *x as i32).collect::<Vec<i32>>().into(),
        )),
        EpicsValue::StringArray(a) => typed(TypedScalarArray::String(a.into())),
        EpicsValue::Int64Array(a) => typed(TypedScalarArray::Long(a.into())),
        EpicsValue::UInt64Array(a) => typed(TypedScalarArray::ULong(a.into())),
        EpicsValue::UShortArray(a) => typed(TypedScalarArray::UShort(a.into())),
        EpicsValue::ULongArray(a) => typed(TypedScalarArray::UInt(a.into())),
        // C `DBF_UCHAR[]` → PVA `ubyte[]` (the common image/byte-buffer shape),
        // pvxs `ioc/typeutils.cpp:34-35`. Element 200 stays 200, not −56.
        EpicsValue::UCharArray(a) => typed(TypedScalarArray::UByte(a.into())),
    }
}

fn typed(t: TypedScalarArray) -> PvField {
    PvField::ScalarArrayTyped(t)
}

/// `EpicsValue` → PVA value-leaf `FieldDesc` (scalar or scalar-array type).
///
/// The type-only projection of [`epics_value_to_pv_leaf`], owning the serve
/// descriptor path (`snapshot_to_field_desc`, after its `DBR_ENUM` → NTEnum
/// intercept). Kept beside the value mapping so the two cannot advertise a
/// different type than they serve.
pub(crate) fn epics_value_to_field_desc_leaf(v: &EpicsValue) -> FieldDesc {
    match v {
        EpicsValue::Double(_) => FieldDesc::Scalar(ScalarType::Double),
        EpicsValue::Float(_) => FieldDesc::Scalar(ScalarType::Float),
        EpicsValue::Long(_) => FieldDesc::Scalar(ScalarType::Int),
        EpicsValue::Short(_) => FieldDesc::Scalar(ScalarType::Short),
        // C `DBF_CHAR` → PVA `byte` (Int8), pvxs `ioc/typeutils.cpp:32-33`.
        EpicsValue::Char(_) => FieldDesc::Scalar(ScalarType::Byte),
        EpicsValue::Enum(_) => FieldDesc::Scalar(ScalarType::Int),
        EpicsValue::EnumWithChoices { .. } => FieldDesc::Scalar(ScalarType::Int),
        EpicsValue::String(_) => FieldDesc::Scalar(ScalarType::String),
        EpicsValue::Int64(_) => FieldDesc::Scalar(ScalarType::Long),
        EpicsValue::UInt64(_) => FieldDesc::Scalar(ScalarType::ULong),
        // C `DBF_USHORT` → PVA `ushort` / `DBF_ULONG` → PVA `uint`
        // (pvxs `ioc/typeutils.cpp:38-44`).
        EpicsValue::UShort(_) => FieldDesc::Scalar(ScalarType::UShort),
        EpicsValue::ULong(_) => FieldDesc::Scalar(ScalarType::UInt),
        // C `DBF_UCHAR` → PVA `ubyte` (pvxs `ioc/typeutils.cpp:34-35`).
        EpicsValue::UChar(_) => FieldDesc::Scalar(ScalarType::UByte),
        EpicsValue::DoubleArray(_) => FieldDesc::ScalarArray(ScalarType::Double),
        EpicsValue::FloatArray(_) => FieldDesc::ScalarArray(ScalarType::Float),
        EpicsValue::LongArray(_) => FieldDesc::ScalarArray(ScalarType::Int),
        EpicsValue::ShortArray(_) => FieldDesc::ScalarArray(ScalarType::Short),
        // C `DBF_CHAR[]` → PVA `byte[]` (Int8), pvxs `ioc/typeutils.cpp:32-33`.
        EpicsValue::CharArray(_) => FieldDesc::ScalarArray(ScalarType::Byte),
        EpicsValue::EnumArray(_) => FieldDesc::ScalarArray(ScalarType::Int),
        EpicsValue::StringArray(_) => FieldDesc::ScalarArray(ScalarType::String),
        EpicsValue::Int64Array(_) => FieldDesc::ScalarArray(ScalarType::Long),
        EpicsValue::UInt64Array(_) => FieldDesc::ScalarArray(ScalarType::ULong),
        EpicsValue::UShortArray(_) => FieldDesc::ScalarArray(ScalarType::UShort),
        EpicsValue::ULongArray(_) => FieldDesc::ScalarArray(ScalarType::UInt),
        // C `DBF_UCHAR[]` → PVA `ubyte[]` (pvxs `ioc/typeutils.cpp:34-35`).
        EpicsValue::UCharArray(_) => FieldDesc::ScalarArray(ScalarType::UByte),
    }
}

/// PVA value-leaf `PvField` → `EpicsValue`, looking through an NT-style
/// structure's `value` member.
///
/// The inverse of [`epics_value_to_pv_leaf`] for the monitor-filter **forward**
/// bridge: it decodes an outbound monitor leaf into the `EpicsValue` the DBR
/// filter engine operates on, so [`epics_value_to_pv_leaf`] can re-emit the
/// transformed leaf on the same wire type. `Byte ↔ Char` and `UByte ↔ UChar`
/// travel as distinct carriers precisely so this round-trip is unambiguous.
///
/// Partial by design: a leaf with no `EpicsValue` counterpart (`boolean`,
/// and shapes with no representable value leaf) returns `None`, which the
/// caller fails closed as a filter incompatible with the negotiated
/// descriptor — never a fabricated stand-in. Every type the serve mapping
/// emits comes back, so a served record can always be filtered.
///
/// Both directions are target-neutral. This backward direction is only ever
/// driven by an inbound PUT in `server_native::tcp`, which used to be
/// host-only and carried this function behind the same gate; `tcp` is now
/// target-neutral itself, so the gate is gone and RTEMS gets the PUT path.
pub(crate) fn pv_leaf_to_epics_value(f: &PvField) -> Option<EpicsValue> {
    fn scalar(sv: &ScalarValue) -> Option<EpicsValue> {
        Some(match sv {
            ScalarValue::Double(d) => EpicsValue::Double(*d),
            ScalarValue::Float(v) => EpicsValue::Float(*v),
            ScalarValue::Int(i) => EpicsValue::Long(*i),
            ScalarValue::Long(l) => EpicsValue::Int64(*l),
            ScalarValue::ULong(u) => EpicsValue::UInt64(*u),
            ScalarValue::Short(s) => EpicsValue::Short(*s),
            // DBF_CHAR is signed (`byte`, Int8) and DBF_UCHAR unsigned
            // (`ubyte`, UInt8) on the wire (pvxs `typeutils.cpp:32-35`); carry
            // each into the filter engine with its own EpicsValue type so
            // `epics_value_to_pv_leaf` can re-emit the correct leaf — folding
            // both into `Char` would make `Byte`(CHAR)↔`UByte`(UCHAR)
            // indistinguishable on the way out. `Char` stores `u8`, so the
            // signed leaf is reinterpreted with `as u8` (wire byte unchanged).
            ScalarValue::Byte(b) => EpicsValue::Char(*b as u8),
            ScalarValue::UByte(b) => EpicsValue::UChar(*b),
            // `ushort` / `uint` are DBF_USHORT / DBF_ULONG, the inverse of
            // the serve mapping above.
            ScalarValue::UShort(u) => EpicsValue::UShort(*u),
            ScalarValue::UInt(u) => EpicsValue::ULong(*u),
            ScalarValue::String(s) => EpicsValue::String(s.clone()),
            ScalarValue::Boolean(_) => return None,
        })
    }
    fn array(items: &[ScalarValue]) -> Option<EpicsValue> {
        // Empty array — default to a Double array (the filter slice of an empty
        // array is still empty, so the element type is irrelevant here).
        let first = items.first();
        Some(match first {
            Some(ScalarValue::Double(_)) | None => EpicsValue::DoubleArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Double(d) => *d,
                        _ => 0.0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Float(_)) => EpicsValue::FloatArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Float(v) => *v,
                        _ => 0.0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Int(_)) => EpicsValue::LongArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Int(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Long(_)) => EpicsValue::Int64Array(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Long(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Short(_)) => EpicsValue::ShortArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Short(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::String(_)) => EpicsValue::StringArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::String(v) => v.clone(),
                        _ => PvString::new(),
                    })
                    .collect(),
            ),
            // a PVA `ulong[]` monitor value must reach the `arr` filter as
            // `UInt64Array` (mirrors the `scalar` helper's `ULong -> UInt64`);
            // without this arm a filtered `DBF_UINT64` waveform fell through to
            // a scalar `Double` and was emitted as an empty `ulong[]` payload.
            Some(ScalarValue::ULong(_)) => EpicsValue::UInt64Array(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::ULong(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            // DBF_CHAR[] (signed byte[]) and DBF_UCHAR[] (unsigned ubyte[])
            // carry distinctly (see the scalar helper): `Byte[] → CharArray`
            // (`as u8`, wire byte unchanged) vs `UByte[] → UCharArray`.
            Some(ScalarValue::Byte(_)) => EpicsValue::CharArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Byte(v) => *v as u8,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::UByte(_)) => EpicsValue::UCharArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::UByte(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::UShort(_)) => EpicsValue::UShortArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::UShort(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::UInt(_)) => EpicsValue::ULongArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::UInt(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Boolean(_)) => return None,
        })
    }
    match f {
        PvField::Scalar(sv) => scalar(sv),
        PvField::ScalarArray(items) => array(items),
        // Wire-decoded and served arrays are the typed form: the `arr`
        // filter gets the same buffer, no elements copied. The element
        // correspondence is the inverse of the serve mapping above.
        PvField::ScalarArrayTyped(t) => Some(match t {
            TypedScalarArray::Double(a) => EpicsValue::DoubleArray(a.to_shared()),
            TypedScalarArray::Float(a) => EpicsValue::FloatArray(a.to_shared()),
            TypedScalarArray::Int(a) => EpicsValue::LongArray(a.to_shared()),
            TypedScalarArray::Long(a) => EpicsValue::Int64Array(a.to_shared()),
            TypedScalarArray::ULong(a) => EpicsValue::UInt64Array(a.to_shared()),
            TypedScalarArray::Short(a) => EpicsValue::ShortArray(a.to_shared()),
            TypedScalarArray::UShort(a) => EpicsValue::UShortArray(a.to_shared()),
            TypedScalarArray::UInt(a) => EpicsValue::ULongArray(a.to_shared()),
            TypedScalarArray::String(a) => EpicsValue::StringArray(a.to_shared()),
            TypedScalarArray::Byte(a) => {
                EpicsValue::CharArray(a.iter().map(|b| *b as u8).collect())
            }
            TypedScalarArray::UByte(a) => EpicsValue::UCharArray(a.to_shared()),
            TypedScalarArray::Boolean(_) => return None,
        }),
        PvField::Structure(s) => s
            .fields
            .iter()
            .find_map(|(k, v)| (k == "value").then_some(v))
            .and_then(pv_leaf_to_epics_value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R17-84: a `histogram`'s bins are C `epicsUInt32` — `cvt_dbaddr`
    /// (`histogramRecord.c:299-308`) sets `field_type = dbr_field_type =
    /// DBF_ULONG` — and pvxs serves DBF_ULONG natively as `uint32[]`
    /// (`ioc/typeutils.cpp:43-44`: `DBR_ULONG -> TypeCode::UInt32`). The port
    /// used to store the bins in an `i32` slot, so PVA introspected VAL as
    /// `int32[]` and a count above `i32::MAX` reached the client negative.
    #[test]
    fn histogram_val_is_served_as_uint32_array() {
        use epics_base_rs::server::record::Record;
        use epics_base_rs::server::records::histogram::HistogramRecord;

        let mut hist = HistogramRecord::new(2, 0.0, 10.0);
        hist.val[0] = 3_000_000_000; // above i32::MAX — the bin that used to go negative
        let val = hist.get_field("VAL").expect("histogram serves VAL");

        assert_eq!(
            epics_value_to_field_desc_leaf(&val),
            FieldDesc::ScalarArray(ScalarType::UInt),
            "DBF_ULONG bins introspect as uint32[]"
        );
        assert_eq!(
            epics_value_to_pv_leaf(&val),
            PvField::ScalarArrayTyped(TypedScalarArray::UInt(vec![3_000_000_000u32, 0].into())),
            "the count crosses the wire unsigned"
        );
    }

    // The DBF_CHAR family: signed CHAR travels as `byte`/`byte[]`, unsigned
    // UCHAR as `ubyte`/`ubyte[]`, in every serve/monitor direction owned here.
    #[test]
    fn dbf_char_is_signed_dbf_uchar_is_unsigned_all_directions() {
        // serve value + monitor backward (EpicsValue → PvField)
        assert_eq!(
            epics_value_to_pv_leaf(&EpicsValue::Char(200)),
            PvField::Scalar(ScalarValue::Byte(-56)),
        );
        assert_eq!(
            epics_value_to_pv_leaf(&EpicsValue::UChar(200)),
            PvField::Scalar(ScalarValue::UByte(200)),
        );
        assert_eq!(
            epics_value_to_pv_leaf(&EpicsValue::CharArray(vec![200, 0].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Byte(vec![-56i8, 0].into())),
        );
        assert_eq!(
            epics_value_to_pv_leaf(&EpicsValue::UCharArray(vec![200].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::UByte(vec![200u8].into())),
        );
        // serve descriptor (EpicsValue → FieldDesc)
        assert_eq!(
            epics_value_to_field_desc_leaf(&EpicsValue::Char(0)),
            FieldDesc::Scalar(ScalarType::Byte),
        );
        assert_eq!(
            epics_value_to_field_desc_leaf(&EpicsValue::UChar(0)),
            FieldDesc::Scalar(ScalarType::UByte),
        );
        assert_eq!(
            epics_value_to_field_desc_leaf(&EpicsValue::CharArray(vec![].into())),
            FieldDesc::ScalarArray(ScalarType::Byte),
        );
        assert_eq!(
            epics_value_to_field_desc_leaf(&EpicsValue::UCharArray(vec![].into())),
            FieldDesc::ScalarArray(ScalarType::UByte),
        );
        // monitor forward (PvField → EpicsValue)
        assert_eq!(
            pv_leaf_to_epics_value(&PvField::Scalar(ScalarValue::Byte(-56))),
            Some(EpicsValue::Char(200)),
        );
        assert_eq!(
            pv_leaf_to_epics_value(&PvField::Scalar(ScalarValue::UByte(200))),
            Some(EpicsValue::UChar(200)),
        );
    }

    // The forward bridge is the inverse of the backward/serve mapping for every
    // carried type: a value round-trips PvField → EpicsValue → PvField with the
    // wire bytes intact. This pins the two owners as genuine inverses so a
    // future edit to one that breaks the correspondence fails here.
    #[test]
    fn forward_is_the_inverse_of_the_serve_backward_mapping() {
        let carried = [
            PvField::Scalar(ScalarValue::Double(1.5)),
            PvField::Scalar(ScalarValue::Float(2.5)),
            PvField::Scalar(ScalarValue::Int(-7)),
            PvField::Scalar(ScalarValue::Long(-9)),
            PvField::Scalar(ScalarValue::ULong(9)),
            PvField::Scalar(ScalarValue::Short(-3)),
            PvField::Scalar(ScalarValue::Byte(-56)),
            PvField::Scalar(ScalarValue::UByte(200)),
            PvField::Scalar(ScalarValue::String(PvString::from("s"))),
            PvField::ScalarArrayTyped(TypedScalarArray::Byte(vec![-56i8, 1].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::UByte(vec![200u8].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::ULong(vec![4u64].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Double(vec![1.5].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Float(vec![2.5f32].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![-7].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Long(vec![-9i64].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::Short(vec![-3i16].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::UShort(vec![3u16].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::UInt(vec![5u32].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::String(vec![PvString::from("s")].into())),
            PvField::Scalar(ScalarValue::UShort(65_535)),
            PvField::Scalar(ScalarValue::UInt(3_000_000_000)),
        ];
        for leaf in carried {
            let ev = pv_leaf_to_epics_value(&leaf).expect("carried type decodes");
            assert_eq!(
                epics_value_to_pv_leaf(&ev),
                leaf,
                "round-trip changed {leaf:?}"
            );
        }
    }

    /// The served leaf is the record's own buffer, and the forward bridge
    /// hands the `arr` filter the wire buffer: neither direction copies.
    #[test]
    fn arrays_are_served_and_bridged_without_copying() {
        let val: epics_base_rs::types::SharedArray<f64> = vec![1.0, 2.0, 3.0].into();
        let leaf = epics_value_to_pv_leaf(&EpicsValue::DoubleArray(val.clone()));
        let PvField::ScalarArrayTyped(TypedScalarArray::Double(a)) = &leaf else {
            panic!("{leaf:?}");
        };
        assert!(std::ptr::eq(a.as_ptr(), val.as_ptr()));
        let Some(EpicsValue::DoubleArray(back)) = pv_leaf_to_epics_value(&leaf) else {
            panic!("no array back");
        };
        assert!(std::ptr::eq(back.as_ptr(), val.as_ptr()));
    }

    // The pre-existing forward-bridge gap: `ushort`/`uint` are not carried (the
    // serve/backward mapping can produce them, but the filter engine drops
    // them). Pinned so a later fill-in is a deliberate, tested change — not
    // silent drift, and not part of the DBF_CHAR family.
    /// A boxed `ushort[]` / `uint[]` leaf (the pre-typed monitor shape)
    /// carries as the same DBF types the typed form does.
    #[test]
    fn boxed_ushort_and_uint_arrays_carry_forward() {
        assert_eq!(
            pv_leaf_to_epics_value(&PvField::ScalarArray(vec![ScalarValue::UShort(7)])),
            Some(EpicsValue::UShortArray(vec![7].into())),
        );
        assert_eq!(
            pv_leaf_to_epics_value(&PvField::ScalarArray(vec![ScalarValue::UInt(9)])),
            Some(EpicsValue::ULongArray(vec![9].into())),
        );
    }
}
