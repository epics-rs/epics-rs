//! `epics:nt/NTNDArray:1.0` — areaDetector image PV.
//!
//! Mirrors the layout from pvxs `nt.h::NTNDArray` and the C++ areaDetector
//! `NDPluginPva` plugin. Produces both a [`FieldDesc`] introspection and a
//! [`PvField`] value composed entirely of native types.
//!
//! Structure:
//!
//! ```text
//! epics:nt/NTNDArray:1.0
//!   union value           // signed-first then unsigned, then float/double
//!     boolean[] booleanValue
//!     byte[]    byteValue
//!     short[]   shortValue
//!     int[]     intValue
//!     long[]    longValue
//!     ubyte[]   ubyteValue
//!     ushort[]  ushortValue
//!     uint[]    uintValue
//!     ulong[]   ulongValue
//!     float[]   floatValue
//!     double[]  doubleValue
//!   codec_t codec
//!     string name
//!     any    parameters
//!   long compressedSize
//!   long uncompressedSize
//!   int uniqueId
//!   time_t dataTimeStamp
//!   alarm_t alarm
//!   time_t timeStamp
//!   dimension_t[] dimension
//!     int     size, offset, fullSize, binning
//!     boolean reverse
//!   epics:nt/NTAttribute:1.0[] attribute
//!     string   name
//!     any      value
//!     string[] tags
//!     string   descriptor
//!     alarm_t  alarm
//!     time_t   timeStamp
//!     int      sourceType
//!     string   source
//! ```

// the module-doc layout block above is kept in sync with
// nt_nd_array_desc()/pvxs nt.cpp:196-251 — it had drifted to a stale
// pre-fix shape (trailing descriptor+display, 5-field attribute).
use crate::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray, VariantValue,
};

/// Per-array data buffer. Caller chooses one variant; the builder produces
/// the corresponding union selector.
#[derive(Debug, Clone)]
pub enum NdArrayBuffer {
    Boolean(Vec<bool>),
    Byte(Vec<i8>),
    UByte(Vec<u8>),
    Short(Vec<i16>),
    UShort(Vec<u16>),
    Int(Vec<i32>),
    UInt(Vec<u32>),
    Long(Vec<i64>),
    ULong(Vec<u64>),
    Float(Vec<f32>),
    Double(Vec<f64>),
}

impl NdArrayBuffer {
    /// Index into the value union (matches the descriptor produced by
    /// [`value_union_desc`]). pvxs `nt.cpp::NTNDArray::build` orders
    /// variants as all signed types first (bool, byte, short, int,
    /// long) then all unsigned (ubyte, ushort, uint, ulong) then
    /// float, double — not ScalarType-enum order.
    pub fn selector(&self) -> i32 {
        match self {
            Self::Boolean(_) => 0,
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::UByte(_) => 5,
            Self::UShort(_) => 6,
            Self::UInt(_) => 7,
            Self::ULong(_) => 8,
            Self::Float(_) => 9,
            Self::Double(_) => 10,
        }
    }

    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Boolean(_) => "booleanValue",
            Self::Byte(_) => "byteValue",
            Self::UByte(_) => "ubyteValue",
            Self::Short(_) => "shortValue",
            Self::UShort(_) => "ushortValue",
            Self::Int(_) => "intValue",
            Self::UInt(_) => "uintValue",
            Self::Long(_) => "longValue",
            Self::ULong(_) => "ulongValue",
            Self::Float(_) => "floatValue",
            Self::Double(_) => "doubleValue",
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Boolean(v) => v.len(),
            Self::Byte(v) => v.len(),
            Self::UByte(v) => v.len(),
            Self::Short(v) => v.len(),
            Self::UShort(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::UInt(v) => v.len(),
            Self::Long(v) => v.len(),
            Self::ULong(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Double(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn element_size_bytes(&self) -> usize {
        match self {
            Self::Boolean(_) | Self::Byte(_) | Self::UByte(_) => 1,
            Self::Short(_) | Self::UShort(_) => 2,
            Self::Int(_) | Self::UInt(_) | Self::Float(_) => 4,
            Self::Long(_) | Self::ULong(_) | Self::Double(_) => 8,
        }
    }

    /// Convert into a `PvField::ScalarArrayTyped`.
    ///
    /// Was `PvField::ScalarArray(Vec<ScalarValue>)`, which boxed every element
    /// into a 24-byte enum: a 640x480 RGB8 frame became a 22 MB vector from
    /// 0.9 MB of pixels, and the encoder then walked it element by element (the
    /// slow arm in `encode.rs` says as much -- "allocator- and CPU-bound").
    /// Measured on a RealSense D405 IOC at 15 fps, that one conversion cost
    /// ~106% CPU for colour and ~30% for depth, and the two tracked element
    /// COUNT rather than byte count, which is the signature of per-element
    /// boxing.
    ///
    /// `ScalarArrayTyped` carries an `Arc<[T]>` the encoder can bulk-memcpy
    /// when host endian matches wire endian. Building it costs one allocation
    /// and one copy, independent of element count.
    pub fn into_scalar_array(self) -> PvField {
        PvField::ScalarArrayTyped(self.into_typed_scalar_array())
    }

    /// Borrowing counterpart of [`Self::into_scalar_array`].
    ///
    /// Lets a caller holding `&NtNdArray` build the value without cloning the
    /// pixel buffer first -- `Arc::from(&[T])` copies once either way, so the
    /// intermediate `Vec` clone was pure overhead.
    pub fn to_scalar_array(&self) -> PvField {
        PvField::ScalarArrayTyped(self.to_typed_scalar_array())
    }

    /// Move the buffer into a [`TypedScalarArray`].
    pub fn into_typed_scalar_array(self) -> TypedScalarArray {
        match self {
            Self::Boolean(v) => TypedScalarArray::Boolean(v.into()),
            Self::Byte(v) => TypedScalarArray::Byte(v.into()),
            Self::UByte(v) => TypedScalarArray::UByte(v.into()),
            Self::Short(v) => TypedScalarArray::Short(v.into()),
            Self::UShort(v) => TypedScalarArray::UShort(v.into()),
            Self::Int(v) => TypedScalarArray::Int(v.into()),
            Self::UInt(v) => TypedScalarArray::UInt(v.into()),
            Self::Long(v) => TypedScalarArray::Long(v.into()),
            Self::ULong(v) => TypedScalarArray::ULong(v.into()),
            Self::Float(v) => TypedScalarArray::Float(v.into()),
            Self::Double(v) => TypedScalarArray::Double(v.into()),
        }
    }

    /// Copy the buffer into a [`TypedScalarArray`] without consuming it.
    pub fn to_typed_scalar_array(&self) -> TypedScalarArray {
        match self {
            Self::Boolean(v) => TypedScalarArray::Boolean(v.as_slice().into()),
            Self::Byte(v) => TypedScalarArray::Byte(v.as_slice().into()),
            Self::UByte(v) => TypedScalarArray::UByte(v.as_slice().into()),
            Self::Short(v) => TypedScalarArray::Short(v.as_slice().into()),
            Self::UShort(v) => TypedScalarArray::UShort(v.as_slice().into()),
            Self::Int(v) => TypedScalarArray::Int(v.as_slice().into()),
            Self::UInt(v) => TypedScalarArray::UInt(v.as_slice().into()),
            Self::Long(v) => TypedScalarArray::Long(v.as_slice().into()),
            Self::ULong(v) => TypedScalarArray::ULong(v.as_slice().into()),
            Self::Float(v) => TypedScalarArray::Float(v.as_slice().into()),
            Self::Double(v) => TypedScalarArray::Double(v.as_slice().into()),
        }
    }

    pub fn variant_field_desc(&self) -> FieldDesc {
        FieldDesc::ScalarArray(match self {
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
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct NdDimension {
    pub size: i32,
    pub offset: i32,
    pub full_size: i32,
    pub binning: i32,
    pub reverse: bool,
}

#[derive(Debug, Clone)]
pub struct NdAttribute {
    pub name: String,
    /// pvxs advertises this member as `Any("value")` (`nt.cpp:240`):
    /// the attribute value is a variant that may carry a scalar, scalar
    /// array, structure, union, or be null. It is stored as a
    /// [`VariantValue`] — the same `any` modeling [`NdCodec::parameters`]
    /// uses — so the public builder can express the full advertised
    /// schema rather than scalars only. Use [`NdAttribute::scalar`] for
    /// the common scalar case.
    pub value: VariantValue,
    /// Per pvxs `nt.cpp:240` NTAttribute element layout includes
    /// a `tags` StringArray between `value` and `descriptor`. Empty
    /// by default; expose to users that need to populate it.
    pub tags: Vec<String>,
    pub descriptor: String,
    pub alarm: NdAlarm,
    pub time_stamp: NdTimeStamp,
    pub source_type: i32,
    pub source: String,
}

impl Default for NdAttribute {
    fn default() -> Self {
        Self {
            name: String::new(),
            // An unset `any` value is the null variant (present member,
            // no value) — not a zero scalar, which would silently narrow
            // the advertised `any` slot to a typed scalar.
            value: VariantValue::null(),
            tags: Vec::new(),
            descriptor: String::new(),
            alarm: NdAlarm::default(),
            time_stamp: NdTimeStamp::default(),
            source_type: 0,
            source: String::new(),
        }
    }
}

impl NdAttribute {
    /// Convenience constructor for the common scalar-valued attribute.
    /// The `any` value carries the scalar's own descriptor, matching what
    /// pvxs emits for a scalar `NTAttribute.value`. For array/structure/
    /// union/null attribute values, set [`NdAttribute::value`] to the
    /// desired [`VariantValue`] directly.
    pub fn scalar(name: impl Into<String>, value: ScalarValue) -> Self {
        Self {
            name: name.into(),
            value: VariantValue::scalar(value),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NdAlarm {
    pub severity: i32,
    pub status: i32,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct NdTimeStamp {
    pub seconds_past_epoch: i64,
    pub nanoseconds: i32,
    pub user_tag: i32,
}

#[derive(Debug, Clone, Default)]
pub struct NdCodec {
    pub name: String,
    /// Codec parameters as a variant value. `None` = empty/no parameters.
    pub parameters: Option<VariantValue>,
}

/// Field order mirrors pvxs `nt.cpp:196-251` `NTNDArray::build()` so the
/// wire descriptor produced by [`nt_nd_array_desc`] is byte-identical to
/// what pvxs's `NTNDArray::build()` emits. Pre-fix Rust had `descriptor`
/// and `display` trailing fields absent from the pvxs schema, and the
/// remaining fields were reordered, so a strict NTNDArray-aware pvxs
/// consumer (e.g. an areaDetector plugin doing
/// `value["uniqueId"].as<int32_t>()`) got a different field position
/// from what its `nt::NTNDArray::create()` expects.
#[derive(Debug, Clone)]
pub struct NtNdArray {
    pub value: NdArrayBuffer,
    pub codec: NdCodec,
    pub compressed_size: i64,
    pub uncompressed_size: i64,
    pub unique_id: i32,
    pub data_time_stamp: NdTimeStamp,
    pub alarm: NdAlarm,
    pub time_stamp: NdTimeStamp,
    pub dimension: Vec<NdDimension>,
    pub attribute: Vec<NdAttribute>,
}

// ── Descriptors ─────────────────────────────────────────────────────────

fn alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "alarm_t".into(),
        fields: vec![
            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

fn time_t_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "time_t".into(),
        fields: vec![
            (
                "secondsPastEpoch".into(),
                FieldDesc::Scalar(ScalarType::Long),
            ),
            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

fn dimension_desc() -> FieldDesc {
    // pvxs `nt.cpp::NTNDArray::build` names this inner struct
    // `dimension_t`; an empty struct_id makes Rust's descriptor
    // tree decode as a different anonymous type than pvxs's.
    FieldDesc::StructureArray {
        struct_id: "dimension_t".into(),
        fields: vec![
            ("size".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("offset".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("fullSize".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("binning".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("reverse".into(), FieldDesc::Scalar(ScalarType::Boolean)),
        ],
    }
}

fn attribute_desc() -> FieldDesc {
    // pvxs `nt.cpp:238-247` field order. Pre-fix Rust omitted
    // `tags`, `alarm`, `timeStamp`.
    FieldDesc::StructureArray {
        struct_id: "epics:nt/NTAttribute:1.0".into(),
        fields: vec![
            ("name".into(), FieldDesc::Scalar(ScalarType::String)),
            ("value".into(), FieldDesc::Variant),
            ("tags".into(), FieldDesc::ScalarArray(ScalarType::String)),
            ("descriptor".into(), FieldDesc::Scalar(ScalarType::String)),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), time_t_desc()),
            ("sourceType".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("source".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

fn codec_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "codec_t".into(),
        fields: vec![
            ("name".into(), FieldDesc::Scalar(ScalarType::String)),
            ("parameters".into(), FieldDesc::Variant),
        ],
    }
}

/// Descriptor of the `value` union (11 typed-array variants).
///
/// pvxs `nt.cpp::NTNDArray::build` orders the variants
/// signed-first then unsigned: bool, byte, short, int, long,
/// ubyte, ushort, uint, ulong, float, double — **not**
/// ScalarType-enum order. Selector indices must match (see
/// [`NdArrayBuffer::selector`]).
pub fn value_union_desc() -> FieldDesc {
    FieldDesc::Union {
        struct_id: String::new(),
        variants: vec![
            (
                "booleanValue".into(),
                FieldDesc::ScalarArray(ScalarType::Boolean),
            ),
            ("byteValue".into(), FieldDesc::ScalarArray(ScalarType::Byte)),
            (
                "shortValue".into(),
                FieldDesc::ScalarArray(ScalarType::Short),
            ),
            ("intValue".into(), FieldDesc::ScalarArray(ScalarType::Int)),
            ("longValue".into(), FieldDesc::ScalarArray(ScalarType::Long)),
            (
                "ubyteValue".into(),
                FieldDesc::ScalarArray(ScalarType::UByte),
            ),
            (
                "ushortValue".into(),
                FieldDesc::ScalarArray(ScalarType::UShort),
            ),
            ("uintValue".into(), FieldDesc::ScalarArray(ScalarType::UInt)),
            (
                "ulongValue".into(),
                FieldDesc::ScalarArray(ScalarType::ULong),
            ),
            (
                "floatValue".into(),
                FieldDesc::ScalarArray(ScalarType::Float),
            ),
            (
                "doubleValue".into(),
                FieldDesc::ScalarArray(ScalarType::Double),
            ),
        ],
    }
}

pub fn nt_nd_array_desc() -> FieldDesc {
    // Field order mirrors pvxs `nt.cpp:196-251` (`NTNDArray::build`).
    FieldDesc::Structure {
        struct_id: "epics:nt/NTNDArray:1.0".into(),
        fields: vec![
            ("value".into(), value_union_desc()),
            ("codec".into(), codec_desc()),
            ("compressedSize".into(), FieldDesc::Scalar(ScalarType::Long)),
            (
                "uncompressedSize".into(),
                FieldDesc::Scalar(ScalarType::Long),
            ),
            ("uniqueId".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("dataTimeStamp".into(), time_t_desc()),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), time_t_desc()),
            ("dimension".into(), dimension_desc()),
            ("attribute".into(), attribute_desc()),
        ],
    }
}

// ── Value builders ──────────────────────────────────────────────────────

fn alarm_value(a: &NdAlarm) -> PvField {
    let mut s = PvStructure::new("alarm_t");
    s.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(a.severity)),
    ));
    s.fields
        .push(("status".into(), PvField::Scalar(ScalarValue::Int(a.status))));
    s.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(a.message.clone().into())),
    ));
    PvField::Structure(s)
}

fn time_t_value(t: &NdTimeStamp) -> PvField {
    let mut s = PvStructure::new("time_t");
    s.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(t.seconds_past_epoch)),
    ));
    s.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(t.nanoseconds)),
    ));
    s.fields.push((
        "userTag".into(),
        PvField::Scalar(ScalarValue::Int(t.user_tag)),
    ));
    PvField::Structure(s)
}

fn dimension_value(dims: &[NdDimension]) -> PvField {
    PvField::StructureArray(
        dims.iter()
            .map(|d| {
                // Paired with `dimension_desc` — must use the same
                // pvxs-canonical struct_id.
                let mut s = PvStructure::new("dimension_t");
                s.fields
                    .push(("size".into(), PvField::Scalar(ScalarValue::Int(d.size))));
                s.fields
                    .push(("offset".into(), PvField::Scalar(ScalarValue::Int(d.offset))));
                s.fields.push((
                    "fullSize".into(),
                    PvField::Scalar(ScalarValue::Int(d.full_size)),
                ));
                s.fields.push((
                    "binning".into(),
                    PvField::Scalar(ScalarValue::Int(d.binning)),
                ));
                s.fields.push((
                    "reverse".into(),
                    PvField::Scalar(ScalarValue::Boolean(d.reverse)),
                ));
                Some(s)
            })
            .collect(),
    )
}

fn attribute_value(attrs: &[NdAttribute]) -> PvField {
    PvField::StructureArray(
        attrs
            .iter()
            .map(|a| {
                let mut s = PvStructure::new("epics:nt/NTAttribute:1.0");
                s.fields.push((
                    "name".into(),
                    PvField::Scalar(ScalarValue::String(a.name.clone().into())),
                ));
                // The attribute `value` is the advertised `any` slot: emit
                // the caller's variant verbatim so a scalar-array,
                // structure, union, or null value reaches the wire intact
                // (pvxs `Any("value")`, nt.cpp:240).
                s.fields
                    .push(("value".into(), PvField::Variant(Box::new(a.value.clone()))));
                s.fields.push((
                    "tags".into(),
                    PvField::ScalarArray(
                        a.tags
                            .iter()
                            .map(|t| ScalarValue::String(t.clone().into()))
                            .collect(),
                    ),
                ));
                s.fields.push((
                    "descriptor".into(),
                    PvField::Scalar(ScalarValue::String(a.descriptor.clone().into())),
                ));
                s.fields.push(("alarm".into(), alarm_value(&a.alarm)));
                s.fields
                    .push(("timeStamp".into(), time_t_value(&a.time_stamp)));
                s.fields.push((
                    "sourceType".into(),
                    PvField::Scalar(ScalarValue::Int(a.source_type)),
                ));
                s.fields.push((
                    "source".into(),
                    PvField::Scalar(ScalarValue::String(a.source.clone().into())),
                ));
                Some(s)
            })
            .collect(),
    )
}

fn codec_value(c: &NdCodec) -> PvField {
    let mut s = PvStructure::new("codec_t");
    s.fields.push((
        "name".into(),
        PvField::Scalar(ScalarValue::String(c.name.clone().into())),
    ));
    let parameters = match &c.parameters {
        Some(v) => PvField::Variant(Box::new(v.clone())),
        None => PvField::Variant(Box::new(VariantValue {
            desc: None,
            value: PvField::Null,
        })),
    };
    s.fields.push(("parameters".into(), parameters));
    PvField::Structure(s)
}

/// Convert an [`NtNdArray`] into a `PvField::Structure` shaped according to
/// [`nt_nd_array_desc`]. Field order mirrors pvxs `nt.cpp:196-251`.
pub fn nt_nd_array_value(nt: &NtNdArray) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTNDArray:1.0");
    // Borrow rather than clone: `to_scalar_array` copies the pixels straight
    // into the `Arc<[T]>`, so the intermediate `Vec` clone this used to make
    // was a second full-frame copy for nothing.
    let union = PvField::Union {
        selector: nt.value.selector(),
        variant_name: nt.value.variant_name().to_string(),
        value: Box::new(nt.value.to_scalar_array()),
    };
    s.fields.push(("value".into(), union));
    s.fields.push(("codec".into(), codec_value(&nt.codec)));
    s.fields.push((
        "compressedSize".into(),
        PvField::Scalar(ScalarValue::Long(nt.compressed_size)),
    ));
    s.fields.push((
        "uncompressedSize".into(),
        PvField::Scalar(ScalarValue::Long(nt.uncompressed_size)),
    ));
    s.fields.push((
        "uniqueId".into(),
        PvField::Scalar(ScalarValue::Int(nt.unique_id)),
    ));
    s.fields
        .push(("dataTimeStamp".into(), time_t_value(&nt.data_time_stamp)));
    s.fields.push(("alarm".into(), alarm_value(&nt.alarm)));
    s.fields
        .push(("timeStamp".into(), time_t_value(&nt.time_stamp)));
    s.fields
        .push(("dimension".into(), dimension_value(&nt.dimension)));
    s.fields
        .push(("attribute".into(), attribute_value(&nt.attribute)));
    PvField::Structure(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_canonical_layout() {
        let d = nt_nd_array_desc();
        match &d {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTNDArray:1.0");
                // pvxs nt.cpp:196-251 produces exactly 10 top-level
                // fields. The earlier `12` reflected the pre-fix
                // Rust shape that included `descriptor` + `display`.
                assert_eq!(fields.len(), 10);
                let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(
                    names,
                    vec![
                        "value",
                        "codec",
                        "compressedSize",
                        "uncompressedSize",
                        "uniqueId",
                        "dataTimeStamp",
                        "alarm",
                        "timeStamp",
                        "dimension",
                        "attribute",
                    ]
                );
            }
            _ => panic!("expected structure"),
        }
    }

    #[test]
    fn value_round_trips_through_encode() {
        use crate::proto::ByteOrder;
        use crate::pvdata::encode::{decode_pv_field, encode_pv_field};
        use std::io::Cursor;

        let nt = NtNdArray {
            value: NdArrayBuffer::UByte(vec![1, 2, 3, 4]),
            codec: NdCodec::default(),
            compressed_size: 4,
            uncompressed_size: 4,
            dimension: vec![NdDimension {
                size: 4,
                ..NdDimension::default()
            }],
            unique_id: 1,
            data_time_stamp: NdTimeStamp::default(),
            alarm: NdAlarm::default(),
            time_stamp: NdTimeStamp::default(),
            attribute: Vec::new(),
        };
        let value = nt_nd_array_value(&nt);
        let desc = nt_nd_array_desc();
        let mut buf = Vec::new();
        encode_pv_field(&value, &desc, ByteOrder::Little, &mut buf);
        let mut cur = Cursor::new(buf.as_slice());
        let _decoded = decode_pv_field(&desc, &mut cur, ByteOrder::Little).unwrap();
    }

    #[test]
    fn attribute_value_carries_non_scalar_variants() {
        // pvxs advertises NTAttribute.value as `Any` (nt.cpp:240).
        // The builder must be able to populate scalar-array, structure,
        // and null attribute values — not just scalars — and they must
        // survive an encode/decode/re-encode round trip against the
        // Variant descriptor.
        use crate::proto::ByteOrder;
        use crate::pvdata::encode::{decode_pv_field, encode_pv_field};
        use std::io::Cursor;

        let scalar_array = VariantValue {
            desc: Some(FieldDesc::ScalarArray(ScalarType::Double)),
            value: PvField::ScalarArray(vec![ScalarValue::Double(1.0), ScalarValue::Double(2.0)]),
        };
        let structure = VariantValue {
            desc: Some(FieldDesc::Structure {
                struct_id: String::new(),
                fields: vec![("n".into(), FieldDesc::Scalar(ScalarType::Int))],
            }),
            value: PvField::Structure({
                let mut s = PvStructure::new("");
                s.fields
                    .push(("n".into(), PvField::Scalar(ScalarValue::Int(7))));
                s
            }),
        };

        let nt = NtNdArray {
            value: NdArrayBuffer::UByte(vec![0]),
            codec: NdCodec::default(),
            compressed_size: 1,
            uncompressed_size: 1,
            unique_id: 1,
            data_time_stamp: NdTimeStamp::default(),
            alarm: NdAlarm::default(),
            time_stamp: NdTimeStamp::default(),
            dimension: Vec::new(),
            attribute: vec![
                NdAttribute {
                    name: "arr".into(),
                    value: scalar_array,
                    ..NdAttribute::default()
                },
                NdAttribute {
                    name: "struct".into(),
                    value: structure,
                    ..NdAttribute::default()
                },
                // Null attribute value — the `any` slot is present but
                // empty (NdAttribute::default uses the null variant).
                NdAttribute {
                    name: "null".into(),
                    ..NdAttribute::default()
                },
                // Scalar convenience constructor still works.
                NdAttribute::scalar("scalar", ScalarValue::Int(3)),
            ],
        };

        let desc = nt_nd_array_desc();
        let value = nt_nd_array_value(&nt);
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut once = Vec::new();
            encode_pv_field(&value, &desc, order, &mut once);
            let mut cur = Cursor::new(once.as_slice());
            let decoded = decode_pv_field(&desc, &mut cur, order)
                .unwrap_or_else(|e| panic!("decode failed ({order:?}): {e:?}"));
            let mut twice = Vec::new();
            encode_pv_field(&decoded, &desc, order, &mut twice);
            assert_eq!(
                once, twice,
                "attribute variant round-trip diverged ({order:?})"
            );
        }
    }
}
