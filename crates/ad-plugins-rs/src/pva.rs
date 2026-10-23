//! NDPluginPva — serves the latest NDArray as an NTNDArray over pvAccess.
//!
//! Corresponds to C++ areaDetector's NDPluginPva. Captures each incoming
//! NDArray, converts it to a native [`PvField::Structure`] shaped per
//! `epics:nt/NTNDArray:1.0`, and posts it to the plugin's [`SharedPV`].

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — the one site ran and
// passed on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 590/590). The reactor it needs is the current-thread
// runtime the test builds itself in order to await `publish`; the backend
// removes no reactor a test owns.

use asyn_rs::port::InterfaceMask;
use std::marker::PhantomData;
use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDataView, NDDataViewMut};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, PortPost, ProcessResult};
use asyn_rs::trace::TraceMask;
use parking_lot::Mutex;

use epics_pva_rs::nt::nd_array::{
    NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
    nt_nd_array_desc, nt_nd_array_value,
};
use epics_pva_rs::proto::BitSet;
use epics_pva_rs::pvdata::encode::default_value_for;
use epics_pva_rs::pvdata::{PvArray, PvField, ScalarValue, Value, VariantValue};
use epics_pva_rs::server_native::SharedPV;

/// The fields NDPluginPvxs's `fromArray` assigns on every frame
/// (ntndArrayConverterPvxs.cpp:200-209, 351-470).
const FROM_ARRAY: [&str; 12] = [
    "value",
    "codec.name",
    "codec.parameters",
    "compressedSize",
    "uncompressedSize",
    "uniqueId",
    "dataTimeStamp.secondsPastEpoch",
    "dataTimeStamp.nanoseconds",
    "timeStamp.secondsPastEpoch",
    "timeStamp.nanoseconds",
    "dimension",
    "attribute",
];

/// Which fields of the served value a frame assigns, as the C server has it.
enum Marking {
    /// NDPluginPva's pvDatabase record answers a GET with bit 0, every field
    /// (channelLocal.cpp:353-356).
    Whole,
    /// NDPluginPvxs's SharedPV, opened on an unmarked `create()`: each post
    /// assigns these bits, [`FROM_ARRAY`]'s.
    FromArray(BitSet),
}

/// PVA plugin processor: captures the latest NDArray, converts it to an
/// NTNDArray [`PvField`], and posts it to its [`SharedPV`], which the IOC
/// adds to the PVA server's `__builtin` store as NDPluginPvxs does
/// (NDPluginPvxs.cpp:50-56).
pub struct PvaProcessor {
    pv_name: String,
    pv: SharedPV,
    plugin_type: &'static str,
    marking: Marking,
}

impl PvaProcessor {
    pub fn new(pv_name: String) -> Self {
        Self::with_plugin_type(pv_name, "NDPluginPva")
    }

    /// `NDPluginPvxs` serves the same NTNDArray under its own PluginType
    /// (NDPluginPvxs.cpp:139).
    pub fn with_plugin_type(pv_name: String, plugin_type: &'static str) -> Self {
        // Opened on an empty NTNDArray, as C's `NTNDArray{}.build().create()`
        // (NDPluginPvxs.cpp:41-52).
        let pv = SharedPV::build_readonly();
        let desc = nt_nd_array_desc();
        let marking = if plugin_type == "NDPluginPvxs" {
            let mut marks = BitSet::new();
            for path in FROM_ARRAY {
                marks.set(desc.bit_for_path(path).expect("an NTNDArray field"));
            }
            pv.open_value(Value::create_from(desc))
                .expect("an empty NTNDArray fits its descriptor");
            Marking::FromArray(marks)
        } else {
            let empty = default_value_for(&desc);
            pv.open(desc, empty)
                .expect("an empty NTNDArray fits its descriptor");
            Marking::Whole
        };
        Self {
            pv_name,
            pv,
            plugin_type,
            marking,
        }
    }

    /// The PV the frames are posted to, for the server to serve.
    pub fn pv(&self) -> SharedPV {
        self.pv.clone()
    }
}

impl Default for PvaProcessor {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl NDPluginProcess for PvaProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_NONE;
    // NDPluginPva.cpp:156, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // NDPluginPvxs.cpp:79-89 counts the drop but still posts, for want of
        // Pva's `else`; both skip the post here.
        if PortPost::of(None).throttled(array) {
            return ProcessResult {
                trace: vec![(
                    TraceMask::WARNING,
                    format!(
                        "{}::processCallbacks maximum byte rate exceeded, dropped array uniqueId={}\n",
                        self.plugin_type, array.unique_id
                    ),
                )],
                counted: false,
                ..ProcessResult::forward(array, vec![])
            };
        }
        let payload = ndarray_to_pv_field(array);

        // A frame that does not fit the NTNDArray descriptor keeps the last
        // good value and reaches no monitor (pvxs `SharedPV::post`).
        let posted = match &self.marking {
            Marking::Whole => self.pv.try_post_checked(payload),
            Marking::FromArray(marks) => self.pv.post_delta(marks, &payload),
        };
        if let Err(err) = posted {
            tracing::warn!(
                pv = %self.pv_name,
                error = %err,
                "dropping NDArray frame that does not match the NTNDArray descriptor"
            );
        }

        // Pass through to downstream plugins.
        ProcessResult::forward(array, vec![])
    }

    fn plugin_type(&self) -> &str {
        self.plugin_type
    }

    /// C `NDPluginPva` passes `compressionAware=true` to the base constructor
    /// (`NDPluginPva.cpp:157`): the NTNDArray value carries the compressed
    /// bytes with `codec`/`compressedSize` describing them, so a compressed
    /// frame is published as it arrives and is never decompressed here.
    /// Without this the runtime drop gate discards every compressed array
    /// before `process_array`, and a `driver -> NDCodec -> Pva` chain serves
    /// the last uncompressed frame forever.
    fn compression_aware(&self) -> bool {
        true
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        let idx = base.create_param("PV_NAME", asyn_rs::param::ParamType::Octet)?;
        base.set_string_param(idx, 0, self.pv_name.clone())?;
        Ok(())
    }

    fn array_data_handle(&self) -> Option<Arc<Mutex<Option<Arc<NDArray>>>>> {
        None
    }
}

// ---------------------------------------------------------------------------
// NDArray → PvField conversion
// ---------------------------------------------------------------------------

fn ndarray_to_pv_field(array: &Arc<NDArray>) -> PvField {
    // C ADCore's NTNDArray converter (ntndArrayConverter.cpp:396-453) applies a
    // uniform rule to compressed and uncompressed arrays: `uncompressedSize` is
    // always the ORIGINAL byte count (NDArrayInfo::totalBytes = nElements *
    // element size of the original type), and `codec.parameters` always records
    // the original scalar type (NDDataTypeToScalar[dataType]) so a consumer can
    // recover it once the value union can no longer carry it. Only the value
    // union arm, `compressedSize`, and `codec.name` differ by branch: a
    // compressed array publishes its raw byte stream under `ubyteValue` with the
    // codec's own `compressedSize`; an uncompressed array uses its type-specific
    // arm, has an empty `codec.name`, and `compressedSize == dataSize`
    // (NDArray.h:136) — which is the pool's requested allocation size, NOT the
    // recomputed `totalBytes`, and is larger whenever a driver over-allocates
    // (`NDArrayPool.cpp:238` sets `compressedSize = dataSize`).
    let original_type = crate::codec::original_data_type(array);
    let num_elements: i64 = if array.dims.is_empty() {
        0
    } else {
        array.dims.iter().map(|d| d.size as i64).product()
    };
    let uncompressed_size = num_elements * original_type.element_size() as i64;
    let codec_parameters = Some(VariantValue::scalar(ScalarValue::Int(
        nd_data_type_to_scalar(original_type),
    )));

    let (value, compressed_size, codec_name) = match &array.codec {
        Some(c) => (
            NdArrayBuffer::UByte(PvArray::from_owner(Arc::new(FrameBytes(Arc::clone(array))))),
            c.compressed_size as i64,
            codec_name_to_string(&c.name),
        ),
        None => (
            frame_pixels(array),
            // C reads `src->compressedSize` on both branches — there is no
            // uncompressed special case (`ntndArrayConverter.cpp:407,410`).
            // `data_size` is the port's `NDArray::dataSize`, which the pool
            // keeps at the larger of the requested and the reused buffer's
            // size, so an over-allocating driver publishes a `compressedSize`
            // above `uncompressedSize` exactly as C does.
            array.data_size as i64,
            String::new(),
        ),
    };
    let codec = NdCodec {
        name: codec_name,
        parameters: codec_parameters,
    };

    let dimension: Vec<NdDimension> = array
        .dims
        .iter()
        .map(|d| NdDimension {
            size: d.size as i32,
            offset: d.offset as i32,
            full_size: d.size as i32,
            // C ntndArrayConverter.cpp:471 puts dims[i].binning verbatim (no clamp).
            binning: d.binning as i32,
            reverse: d.reverse,
        })
        .collect();

    // C ADCore fills the two NTNDArray time fields from two distinct sources
    // (ntndArrayConverter.cpp:477-503): `dataTimeStamp` from the floating-point
    // `NDArray::timeStamp` (fromDataTimeStamp), `timeStamp` from the integer
    // `NDArray::epicsTS` (fromTimeStamp). Both add POSIX_TIME_AT_EPICS_EPOCH.
    let data_time_stamp = double_ts_to_nt(array.time_stamp);
    let time_stamp = epics_ts_to_nt(&array.timestamp);

    let attribute: Vec<NdAttribute> = array
        .attributes
        .iter()
        .map(|a| NdAttribute {
            name: a.name.clone(),
            value: attribute_value_to_variant(&a.value),
            tags: Vec::new(),
            descriptor: a.description.clone(),
            alarm: NdAlarm::default(),
            // C ADCore converters (ntndArrayConverter.cpp:544-590,
            // ntndArrayConverterPvxs.cpp::fromAttributes) populate only
            // name/descriptor/source/sourceType/value on each attribute;
            // `NDAttribute` carries no timestamp, so the per-attribute
            // `timeStamp` stays at the NTAttribute default. Do NOT copy the
            // image/array timestamp here — that would fabricate per-attribute
            // provenance C never emits.
            time_stamp: NdTimeStamp::default(),
            source_type: ndattr_source_type(&a.source),
            // C publishes `NDAttribute::getSource()` verbatim
            // (ntndArrayConverter.cpp:564); never synthesize it from the type.
            source: a.source.source_string().to_string(),
        })
        .collect();

    let nt = NtNdArray {
        value,
        codec,
        compressed_size,
        uncompressed_size,
        dimension,
        unique_id: array.unique_id,
        data_time_stamp,
        attribute,
        alarm: NdAlarm {
            severity: 0,
            status: 0,
            message: "NO_ALARM".into(),
        },
        time_stamp,
    };
    nt_nd_array_value(&nt)
}

/// The frame itself, published as the owner of its pixel slice. C wraps
/// `pData` in a `shared_vector` whose deleter releases the NDArray
/// (ntndArrayConverter.cpp:423-429), so the value union carries the
/// driver's buffer and no copy; holding the `Arc` is the same contract, the
/// pool gets the buffer back when the last monitor snapshot drops it.
struct Pixels<T>(Arc<NDArray>, PhantomData<T>);

macro_rules! pixel_owner {
    ($t:ty, $variant:ident) => {
        impl AsRef<[$t]> for Pixels<$t> {
            fn as_ref(&self) -> &[$t] {
                match self.0.data.view() {
                    NDDataView::$variant(v) => v,
                    _ => unreachable!("Pixels<T> is built from the matching NDDataBuffer variant"),
                }
            }
        }
    };
}
pixel_owner!(i8, I8);
pixel_owner!(u8, U8);
pixel_owner!(i16, I16);
pixel_owner!(u16, U16);
pixel_owner!(i32, I32);
pixel_owner!(u32, U32);
pixel_owner!(i64, I64);
pixel_owner!(u64, U64);
pixel_owner!(f32, F32);
pixel_owner!(f64, F64);

/// The frame as the owner of its raw byte view, for the compressed branch's
/// `ubyteValue` arm.
struct FrameBytes(Arc<NDArray>);

impl AsRef<[u8]> for FrameBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.data.as_u8_slice()
    }
}

fn frame_pixels(array: &Arc<NDArray>) -> NdArrayBuffer {
    fn own<T>(array: &Arc<NDArray>) -> PvArray<T>
    where
        T: Send + Sync + 'static,
        Pixels<T>: AsRef<[T]>,
    {
        PvArray::from_owner(Arc::new(Pixels(Arc::clone(array), PhantomData)))
    }
    match array.data.view() {
        NDDataView::I8(_) => NdArrayBuffer::Byte(own(array)),
        NDDataView::U8(_) => NdArrayBuffer::UByte(own(array)),
        NDDataView::I16(_) => NdArrayBuffer::Short(own(array)),
        NDDataView::U16(_) => NdArrayBuffer::UShort(own(array)),
        NDDataView::I32(_) => NdArrayBuffer::Int(own(array)),
        NDDataView::U32(_) => NdArrayBuffer::UInt(own(array)),
        NDDataView::I64(_) => NdArrayBuffer::Long(own(array)),
        NDDataView::U64(_) => NdArrayBuffer::ULong(own(array)),
        NDDataView::F32(_) => NdArrayBuffer::Float(own(array)),
        NDDataView::F64(_) => NdArrayBuffer::Double(own(array)),
    }
}

/// Convert an EPICS-epoch `epicsTS` into the NT `timeStamp` `time_t`, mirroring
/// `NTNDArrayConverter::fromTimeStamp` (ntndArrayConverter.cpp:493-501):
/// `TimeStamp(epicsTS.secPastEpoch + POSIX_TIME_AT_EPICS_EPOCH, epicsTS.nsec)`.
/// The C `TimeStamp` ctor leaves `userTag` at 0.
fn epics_ts_to_nt(ts: &ad_core_rs::timestamp::EpicsTimestamp) -> NdTimeStamp {
    NdTimeStamp {
        seconds_past_epoch: ts.sec as i64 + ad_core_rs::timestamp::EPICS_EPOCH_OFFSET as i64,
        nanoseconds: ts.nsec as i32,
        user_tag: 0,
    }
}

/// Convert the floating-point `NDArray::timeStamp` (seconds since the EPICS
/// epoch) into the NT `dataTimeStamp` `time_t`, mirroring
/// `NTNDArrayConverter::fromDataTimeStamp` (ntndArrayConverter.cpp:477-490):
/// floor to whole seconds, scale the fraction to nanoseconds, then add
/// POSIX_TIME_AT_EPICS_EPOCH. The C `TimeStamp` ctor leaves `userTag` at 0.
fn double_ts_to_nt(t: f64) -> NdTimeStamp {
    let seconds = t.floor();
    let nanoseconds = ((t - seconds) * 1e9) as i32;
    NdTimeStamp {
        seconds_past_epoch: seconds as i64 + ad_core_rs::timestamp::EPICS_EPOCH_OFFSET as i64,
        nanoseconds,
        user_tag: 0,
    }
}

fn codec_name_to_string(name: &ad_core_rs::codec::CodecName) -> String {
    name.as_str().to_string()
}

/// pvData `ScalarType` int for an NDArray element type — the value C writes into
/// `codec.parameters` so a consumer can recover the original element type after
/// decompression (C `NDDataTypeToScalar`, ntndArrayConverter.cpp:25-36). The
/// pvData ScalarType enum order is pvBoolean=0, pvByte=1, pvShort=2, pvInt=3,
/// pvLong=4, pvUByte=5, pvUShort=6, pvUInt=7, pvULong=8, pvFloat=9, pvDouble=10.
fn nd_data_type_to_scalar(dt: NDDataType) -> i32 {
    match dt {
        NDDataType::Int8 => 1,     // pvByte
        NDDataType::UInt8 => 5,    // pvUByte
        NDDataType::Int16 => 2,    // pvShort
        NDDataType::UInt16 => 6,   // pvUShort
        NDDataType::Int32 => 3,    // pvInt
        NDDataType::UInt32 => 7,   // pvUInt
        NDDataType::Int64 => 4,    // pvLong
        NDDataType::UInt64 => 8,   // pvULong
        NDDataType::Float32 => 9,  // pvFloat
        NDDataType::Float64 => 10, // pvDouble
    }
}

/// `sourceType` field for the NTNDArray attribute — the raw `NDAttrSource_t`
/// enum int that C writes verbatim (`ntndArrayConverter.cpp:566-567`,
/// enum order `NDAttribute.h:62-68`).
fn ndattr_source_type(src: &ad_core_rs::attributes::NDAttrSource) -> i32 {
    use ad_core_rs::attributes::NDAttrSource;
    match src {
        NDAttrSource::Driver => 0,
        NDAttrSource::Param { .. } => 1,
        NDAttrSource::EpicsPV(_) => 2,
        NDAttrSource::Function(_) => 3,
        NDAttrSource::Constant(_) => 4,
        NDAttrSource::Undefined => 5,
    }
}

/// Convert an areaDetector attribute value into the `any` variant that
/// fills the advertised `Any("value")` NTAttribute slot (pvxs
/// `nt.cpp:240-247`). A defined value carries its own scalar descriptor;
/// `Undefined` becomes the null variant (slot present, no value) rather
/// than masquerading as a zero `int`, which would silently narrow the
/// advertised `any` to a typed scalar.
fn attribute_value_to_variant(val: &ad_core_rs::attributes::NDAttrValue) -> VariantValue {
    use ad_core_rs::attributes::NDAttrValue;
    let scalar = match val {
        NDAttrValue::Int8(v) => ScalarValue::Byte(*v),
        NDAttrValue::UInt8(v) => ScalarValue::UByte(*v),
        NDAttrValue::Int16(v) => ScalarValue::Short(*v),
        NDAttrValue::UInt16(v) => ScalarValue::UShort(*v),
        NDAttrValue::Int32(v) => ScalarValue::Int(*v),
        NDAttrValue::UInt32(v) => ScalarValue::UInt(*v),
        NDAttrValue::Int64(v) => ScalarValue::Long(*v),
        NDAttrValue::UInt64(v) => ScalarValue::ULong(*v),
        NDAttrValue::Float32(v) => ScalarValue::Float(*v),
        NDAttrValue::Float64(v) => ScalarValue::Double(*v),
        NDAttrValue::String(v) => ScalarValue::String(v.clone().into()),
        NDAttrValue::Undefined => return VariantValue::null(),
    };
    VariantValue::scalar(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};
    use epics_pva_rs::pvdata::{ScalarType, TypedScalarArray};

    #[test]
    fn convert_simple_array() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 42;
        if let NDDataViewMut::U8(buf) = arr.data.view_mut() {
            for (i, v) in buf.iter_mut().enumerate() {
                *v = i as u8;
            }
        }
        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        match &payload {
            PvField::Structure(s) => {
                assert_eq!(s.struct_id, "epics:nt/NTNDArray:1.0");
                assert!(s.get_field("value").is_some());
                assert!(s.get_field("dimension").is_some());
            }
            _ => panic!("expected structure"),
        }
    }

    /// C publishes `pData` itself (ntndArrayConverter.cpp:423-429): the value
    /// arm must point at the frame's buffer and keep the frame alive, not at
    /// a copy.
    #[test]
    fn the_published_value_is_the_frame_buffer_itself() {
        let arr = Arc::new(NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt16,
        ));
        let NDDataView::U16(pixels) = arr.data.view() else {
            panic!("expected u16 pixels");
        };
        let frame_ptr = pixels.as_ptr();

        let payload = ndarray_to_pv_field(&arr);
        assert_eq!(Arc::strong_count(&arr), 2, "the payload holds the frame");
        let PvField::Structure(s) = &payload else {
            panic!("expected structure");
        };
        let Some(PvField::Union { value, .. }) = s.get_field("value") else {
            panic!("expected value union");
        };
        let PvField::ScalarArrayTyped(TypedScalarArray::UShort(items)) = value.as_ref() else {
            panic!("expected ushort scalar array, got {value:?}");
        };
        assert_eq!(items.as_ptr(), frame_ptr);
        assert_eq!(items.len(), 64 * 64);

        drop(arr);
        assert_eq!(items.len(), 64 * 64, "the payload keeps the frame alive");
    }

    /// The compressed branch publishes the frame's byte stream the same way.
    #[test]
    fn the_published_compressed_bytes_are_the_frame_buffer_itself() {
        let arr = NDArray::new(vec![NDDimension::new(64)], NDDataType::UInt16);
        let arr = Arc::new(crate::codec::compress_lz4(&arr).unwrap());
        let frame_ptr = arr.data.as_u8_slice().as_ptr();

        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure");
        };
        let Some(PvField::Union { value, .. }) = s.get_field("value") else {
            panic!("expected value union");
        };
        let PvField::ScalarArrayTyped(TypedScalarArray::UByte(items)) = value.as_ref() else {
            panic!("expected ubyte scalar array, got {value:?}");
        };
        assert_eq!(items.as_ptr(), frame_ptr);
    }

    /// Regression: the NTNDArray `dimension[].binning` field must serialize the
    /// raw NDDimension binning verbatim. C ntndArrayConverter.cpp:471 does
    /// `put(src->dims[i].binning)` with no clamp, so a binning of 0 goes out as
    /// 0. Pre-fix Rust applied `.max(1)`, turning a 0 into a wire 1.
    #[test]
    fn dimension_binning_serializes_zero_verbatim() {
        let mut dim = NDDimension::new(8);
        dim.binning = 0;
        let arr = NDArray::new(vec![dim], NDDataType::UInt8);

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };
        let Some(PvField::StructureArray(dims)) = s.get_field("dimension") else {
            panic!("expected dimension StructureArray");
        };
        let d0 = dims[0].as_ref().expect("dimension[0] present");
        assert_eq!(
            d0.get_field("binning"),
            Some(&PvField::Scalar(ScalarValue::Int(0))),
            "binning 0 must stay 0 on the wire, not clamp to 1"
        );
    }

    /// Regression for the compressed NTNDArray wire contract.
    ///
    /// C ADCore's NTNDArray converter publishes a compressed array as a distinct
    /// wire shape (ntndArrayConverter.cpp:407-453): the compressed byte stream
    /// goes out under `value.ubyteValue` with `compressedSize` bytes,
    /// `uncompressedSize` stays the ORIGINAL uncompressed byte count, and
    /// `codec.parameters` holds the original scalar type
    /// (`NDDataTypeToScalar[dataType]`, pvUShort=6 for UInt16) because the value
    /// union selector is now ubyte. Pre-fix Rust chose the union arm before
    /// checking the codec, so a compressed UInt16 array stayed `ushortValue`
    /// with a length/type taken from the compressed byte buffer and a null
    /// `codec.parameters`.
    #[test]
    fn compressed_array_emits_ubyte_value_and_codec_parameters() {
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        if let NDDataViewMut::U16(buf) = arr.data.view_mut() {
            for (i, v) in buf.iter_mut().enumerate() {
                *v = (i * 1000) as u16;
            }
        }
        let uncompressed_bytes = (arr.data.len() * 2) as i64; // 8 elems * 2 bytes

        let compressed = crate::codec::compress_lz4(&arr).unwrap();
        let comp_size = compressed.codec.as_ref().unwrap().compressed_size as i64;
        assert!(
            matches!(compressed.data.view(), NDDataView::U8(_)),
            "compressed buffer must hold raw bytes"
        );

        let compressed = Arc::new(compressed);
        let payload = ndarray_to_pv_field(&compressed);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };

        // value must be the ubyteValue union arm with exactly compressed_size bytes.
        let Some(PvField::Union {
            variant_name,
            value,
            ..
        }) = s.get_field("value")
        else {
            panic!("expected value union");
        };
        assert_eq!(variant_name, "ubyteValue");
        // pvxs declares this union arm `UInt8A("ubyteValue")` (nt.cpp:214), a
        // concrete uint8 array — pvxs has no untyped-array type at all — so the
        // emitter's `ScalarArrayTyped` is the wire shape and the element type
        // is part of the contract being asserted, not an incidental
        // representation.
        match value.as_ref() {
            PvField::ScalarArrayTyped(items) => {
                assert_eq!(items.len() as i64, comp_size);
                assert_eq!(items.scalar_type(), ScalarType::UByte);
            }
            other => panic!("expected ubyte scalar array, got {other:?}"),
        }

        assert_eq!(
            s.get_field("compressedSize"),
            Some(&PvField::Scalar(ScalarValue::Long(comp_size)))
        );
        // uncompressedSize is the ORIGINAL byte count, not the compressed length.
        assert_eq!(
            s.get_field("uncompressedSize"),
            Some(&PvField::Scalar(ScalarValue::Long(uncompressed_bytes)))
        );

        let Some(PvField::Structure(codec)) = s.get_field("codec") else {
            panic!("expected codec structure");
        };
        assert_eq!(
            codec.get_field("name"),
            Some(&PvField::Scalar(ScalarValue::String("lz4".into())))
        );
        // codec.parameters carries the original scalar type: pvUShort = 6.
        let Some(PvField::Variant(params)) = codec.get_field("parameters") else {
            panic!("expected codec.parameters variant");
        };
        assert_eq!(params.value, PvField::Scalar(ScalarValue::Int(6)));
        assert!(
            params.desc.is_some(),
            "codec.parameters must carry a descriptor (non-null variant)"
        );
    }

    /// ADP-85. C reads `src->compressedSize` for BOTH branches — there is no
    /// uncompressed special case (`ntndArrayConverter.cpp:407,410`) — and the
    /// pool sets `compressedSize = dataSize`, the requested allocation size
    /// (`NDArrayPool.cpp:238`, contract at `NDArray.h:136`). When a driver
    /// over-allocates, that exceeds the recomputed `totalBytes` and C publishes
    /// a wire `compressedSize` LARGER than `uncompressedSize`. The port used to
    /// emit `uncompressedSize` on the uncompressed branch, so the two were
    /// always equal and the over-allocation was invisible to a client.
    #[test]
    fn uncompressed_compressed_size_is_the_arrays_data_size() {
        use ad_core_rs::ndarray_pool::NDArrayPool;

        let pool = NDArrayPool::new(0);
        // Allocate 100 bytes, hand it back, then ask for 80. The pool reuses
        // the 100-byte buffer and keeps the larger tracked `data_size`, which
        // is the port's `NDArray::dataSize` — the over-allocating-driver case.
        let big = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        pool.release(big);
        let arr = pool
            .alloc(vec![NDDimension::new(80)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(arr.data_size, 100, "pool must keep the reused buffer size");
        assert!(arr.codec.is_none());

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };
        // uncompressedSize is the recomputed original byte count: 80 * 1.
        assert_eq!(
            s.get_field("uncompressedSize"),
            Some(&PvField::Scalar(ScalarValue::Long(80)))
        );
        // compressedSize is the NDArray's own dataSize, which is larger.
        assert_eq!(
            s.get_field("compressedSize"),
            Some(&PvField::Scalar(ScalarValue::Long(100)))
        );
    }

    /// Regression: a compressed frame's NTNDArray `attribute[]` must hold only
    /// genuine driver/user attributes. C ADCore keeps the uncompressed element
    /// type in the native `NDArray::dataType` field (NDPluginCodec.cpp:35,70),
    /// never as an NDAttribute; the Rust port records it in
    /// `Codec::original_data_type`, so the compressor attaches NO carrier
    /// attribute and the converter maps every attribute through unfiltered.
    #[test]
    fn compressed_frame_attribute_list_omits_codec_carrier() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        if let NDDataViewMut::U16(buf) = arr.data.view_mut() {
            for (i, v) in buf.iter_mut().enumerate() {
                *v = (i * 1000) as u16;
            }
        }
        // A genuine driver attribute that MUST be published.
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color Mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(0),
        ));

        // The compressor records the original type structurally in the codec
        // and attaches no carrier attribute.
        let compressed = crate::codec::compress_lz4(&arr).unwrap();
        assert_eq!(
            compressed.codec.as_ref().unwrap().original_data_type,
            NDDataType::UInt16,
            "precondition: the original element type is recorded in the codec"
        );
        assert!(
            compressed
                .attributes
                .get("CODEC_ORIGINAL_DATA_TYPE")
                .is_none(),
            "precondition: no carrier attribute is attached to the array"
        );

        let compressed = Arc::new(compressed);
        let payload = ndarray_to_pv_field(&compressed);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };
        let Some(PvField::StructureArray(attrs)) = s.get_field("attribute") else {
            panic!("expected attribute structure-array");
        };

        let names: Vec<String> = attrs
            .iter()
            .filter_map(|a| a.as_ref())
            .filter_map(|a| match a.get_field("name") {
                Some(PvField::Scalar(ScalarValue::String(v))) => {
                    Some(v.as_str_lossy().into_owned())
                }
                _ => None,
            })
            .collect();

        assert!(
            !names.iter().any(|n| n == "CODEC_ORIGINAL_DATA_TYPE"),
            "internal codec carrier must not appear in NTNDArray attribute[]; got {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "ColorMode"),
            "genuine driver attributes must still be published; got {names:?}"
        );
    }

    /// The served value's `uniqueId`.
    fn served_unique_id(pv: &SharedPV) -> Option<i32> {
        match pv.current()? {
            PvField::Structure(s) => match s.get_field("uniqueId") {
                Some(PvField::Scalar(ScalarValue::Int(id))) => Some(*id),
                _ => None,
            },
            _ => None,
        }
    }

    /// The fields a GET of `pv` frames: the server's `marked` set, `None`
    /// for all of them.
    fn served_marks(pv: SharedPV) -> Option<Vec<String>> {
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};
        use epics_pva_rs::server_native::{ChannelSource, SharedSource};
        let src = SharedSource::new();
        src.add("M", pv);
        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };
        epics_base_rs::runtime::task::test_block_on(async {
            let checked = AccessGate::open()
                .check("M", "127.0.0.1", "anonymous", "anonymous", "")
                .await;
            src.read_checked(checked, ctx).await.expect("open").marked
        })
    }

    /// C NDPluginPvxs opens its SharedPV on `NTNDArray{}.build().create()`,
    /// which marks nothing, so a GET before the first frame frames no field
    /// (NDPluginPvxs.cpp:41-52): the PUT's fetch a pvxs client assigns from
    /// must not carry the unselected `value` union (clientget.cpp:561).
    #[test]
    fn an_ndpvxs_pv_reads_no_field_before_its_first_frame() {
        let proc = PvaProcessor::with_plugin_type("M".into(), "NDPluginPvxs");
        assert_eq!(served_marks(proc.pv()), Some(Vec::new()));
    }

    /// Each NDPluginPvxs frame marks exactly what `fromArray` assigns
    /// (ntndArrayConverterPvxs.cpp:200-209, 351-470).
    #[test]
    fn an_ndpvxs_frame_marks_what_from_array_assigns() {
        let proc = PvaProcessor::with_plugin_type("M".into(), "NDPluginPvxs");
        let arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        proc.process_array(&Arc::new(arr), &NDArrayPool::new(1_000_000));
        let mut got = served_marks(proc.pv()).expect("a field subset");
        got.sort();
        assert_eq!(
            got,
            [
                "attribute",
                "codec.name",
                "codec.parameters",
                "compressedSize",
                "dataTimeStamp.nanoseconds",
                "dataTimeStamp.secondsPastEpoch",
                "dimension",
                "timeStamp.nanoseconds",
                "timeStamp.secondsPastEpoch",
                "uncompressedSize",
                "uniqueId",
                "value",
            ]
        );
    }

    /// NDPluginPva's pvDatabase record answers every GET with bit 0
    /// (channelLocal.cpp:353-356): all fields, before a frame or after.
    #[test]
    fn an_ndpva_pv_reads_every_field() {
        let proc = PvaProcessor::new("M".into());
        assert_eq!(served_marks(proc.pv()), None);
        let arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        proc.process_array(&Arc::new(arr), &NDArrayPool::new(1_000_000));
        assert_eq!(served_marks(proc.pv()), None);
    }

    /// A real NDArray frame must fit the `nt_nd_array_desc()` the PV is
    /// opened with, or every frame is refused and the PV keeps serving the
    /// empty NTNDArray it opened on.
    #[test]
    fn processor_stores_latest() {
        let proc = PvaProcessor::new("TEST:Pva1:Image".into());
        assert_eq!(served_unique_id(&proc.pv()), Some(0));
        let pool = NDArrayPool::new(1_000_000);
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        arr.unique_id = 5;
        proc.process_array(&Arc::new(arr), &pool);

        assert_eq!(served_unique_id(&proc.pv()), Some(5));
    }

    #[test]
    fn compressed_frame_reaches_the_pv_instead_of_being_dropped() {
        // C NDPluginPva passes compressionAware=true (NDPluginPva.cpp:157), so
        // the runtime admission gate (`plugin/channel.rs` `classify`:
        // `codec.is_some() && !compression_aware` => DropCompressed) must let a
        // compressed frame through. Without the override a
        // driver -> NDCodec -> Pva chain publishes nothing and the PV keeps
        // serving the last uncompressed frame. The wire-shape tests above call
        // `ndarray_to_pv_field` directly and never cross the gate, so only a
        // runtime-level publish shows the drop.
        use ad_core_rs::plugin::runtime::create_plugin_runtime;
        use ad_core_rs::plugin::wiring::WiringRegistry;
        use std::sync::atomic::Ordering;

        let mut raw = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        raw.unique_id = 1;
        let compressed = crate::codec::compress_lz4(&raw).unwrap();
        assert!(compressed.codec.is_some(), "input must carry a codec");

        let processor = PvaProcessor::new("TEST:Pva1:Image".into());
        let pv = processor.pv();
        let pool = NDArrayPool::new(1_000_000);
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PVA1").unwrap(),
            processor,
            pool.max_memory(),
            10,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let dropped = handle.array_sender().dropped_arrays_counter().clone();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        // Fence: the write only queues the enable for the data thread.
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply EnableCallbacks"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(
            handle.array_sender().publish(
                ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                    .alloc_copy(&compressed)
                    .unwrap(),
            ),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let served = loop {
            if served_unique_id(&pv) == Some(1) {
                break pv.current().unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the compressed frame never reached the PV (dropped={})",
                dropped.load(Ordering::Acquire)
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "compression-aware Pva must not count its compressed input as dropped"
        );

        let PvField::Structure(s) = served else {
            panic!("expected an NTNDArray structure, got {served:?}");
        };
        let Some(PvField::Structure(codec)) = s.get_field("codec") else {
            panic!("expected codec structure");
        };
        assert_eq!(
            codec.get_field("name"),
            Some(&PvField::Scalar(ScalarValue::String("lz4".into()))),
            "the served snapshot must be the compressed frame"
        );
    }

    /// C skips `m_record->update` for a frame `throttled()` refuses, counts
    /// it in DroppedOutputArrays and takes it back off ArrayCounter
    /// (NDPluginPva.cpp:99-115).
    #[test]
    fn a_throttled_frame_is_not_posted() {
        use ad_core_rs::plugin::runtime::create_plugin_runtime;
        use ad_core_rs::plugin::wiring::WiringRegistry;

        let processor = PvaProcessor::new("TEST:PvaThrottle:Image".into());
        let pv = processor.pv();
        let pool = NDArrayPool::new(1_000_000);
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PVA_THROTTLE").unwrap(),
            processor,
            pool.max_memory(),
            10,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle().clone();
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        // One 16-byte frame fills the bucket; the refill over the run is
        // a fraction of a byte.
        port.write_float64_blocking(handle.plugin_params.max_byte_rate, 0, 16.0)
            .unwrap();
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));

        // RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — passes on the exec
        // backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
        // ad-plugins-rs --all-features a_throttled_frame_is_not_posted`); the
        // runtime is the test's own, built to await `publish`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let driver = ad_core_rs::ndarray_pool::DriverPool::standalone(0);
        for id in 1..=3 {
            let mut frame = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
            frame.unique_id = id;
            rt.block_on(
                handle
                    .array_sender()
                    .publish(driver.alloc_copy(&frame).unwrap()),
            );
        }

        let dropped = handle.plugin_params.dropped_output_arrays;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while port.read_int32_blocking(dropped, 0).ok() != Some(2)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(port.read_int32_blocking(dropped, 0).unwrap(), 2);
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            1
        );
        assert_eq!(served_unique_id(&pv), Some(1));
    }

    #[test]
    fn attribute_values_fill_any_slot() {
        // The NTAttribute `value` is the advertised `Any("value")` slot
        // (pvxs nt.cpp:240-247). A defined areaDetector attribute must
        // reach the wire as a tagged scalar variant; an `Undefined`
        // attribute must be the null variant (slot present, no value), not
        // a zero `int` that silently narrows the `any` to a typed scalar.
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let mut arr = NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "gain",
            "detector gain",
            NDAttrSource::Constant(String::new()),
            NDAttrValue::Int32(7),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "missing",
            "undefined attr",
            NDAttrSource::Undefined,
            NDAttrValue::Undefined,
        ));

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };
        let Some(PvField::StructureArray(attrs)) = s.get_field("attribute") else {
            panic!("expected attribute structure-array");
        };
        assert_eq!(attrs.len(), 2);

        let gain = attrs[0].as_ref().expect("gain attribute present");
        match gain.get_field("value") {
            Some(PvField::Variant(vv)) => {
                assert_eq!(vv.value, PvField::Scalar(ScalarValue::Int(7)));
                assert!(vv.desc.is_some(), "defined value must carry a descriptor");
            }
            other => panic!("expected variant gain value, got {other:?}"),
        }

        let missing = attrs[1].as_ref().expect("undefined attribute present");
        match missing.get_field("value") {
            Some(PvField::Variant(vv)) => {
                assert_eq!(vv.value, PvField::Null);
                assert!(
                    vv.desc.is_none(),
                    "undefined value must be the null variant"
                );
            }
            other => panic!("expected null variant value, got {other:?}"),
        }
    }

    /// Regression.
    ///
    /// C ADCore converters set only name/descriptor/source/sourceType/value
    /// on each NTAttribute (ntndArrayConverterPvxs.cpp::fromAttributes);
    /// `NDAttribute` carries no timestamp, so the per-attribute `timeStamp`
    /// stays at the NTAttribute default while top-level `dataTimeStamp`/
    /// `timeStamp` follow the image source. Pre-fix Rust copied the image
    /// timestamp into every attribute's `timeStamp`, fabricating per-
    /// attribute provenance C never emits.
    #[test]
    fn attribute_timestamp_stays_default_not_image_ts() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use epics_pva_rs::pvdata::PvStructure;

        let mut arr = NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt8);
        arr.time_stamp = 1234.0;
        arr.timestamp.sec = 1234;
        arr.timestamp.nsec = 567;
        arr.attributes.add(NDAttribute::new_static(
            "gain",
            "detector gain",
            NDAttrSource::Constant(String::new()),
            NDAttrValue::Int32(7),
        ));

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };

        // Read (secondsPastEpoch, nanoseconds) from a `time_t` member.
        let read_ts = |st: &PvStructure, field: &str| -> (i64, i32) {
            let Some(PvField::Structure(ts)) = st.get_field(field) else {
                panic!("expected {field} time_t structure");
            };
            let sec = match ts.get_field("secondsPastEpoch") {
                Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
                other => panic!("expected secondsPastEpoch Long, got {other:?}"),
            };
            let nsec = match ts.get_field("nanoseconds") {
                Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
                other => panic!("expected nanoseconds Int, got {other:?}"),
            };
            (sec, nsec)
        };

        // Top-level timestamps follow the image source, with the EPICS->POSIX
        // offset added: dataTimeStamp from the
        // floating-point `time_stamp`, timeStamp from the integer `epicsTS`.
        let offset = ad_core_rs::timestamp::EPICS_EPOCH_OFFSET as i64;
        assert_eq!(read_ts(s, "dataTimeStamp"), (1234 + offset, 0));
        assert_eq!(read_ts(s, "timeStamp"), (1234 + offset, 567));

        // Per-attribute timeStamp must stay default (0, 0), NOT the
        // fabricated image timestamp.
        let Some(PvField::StructureArray(attrs)) = s.get_field("attribute") else {
            panic!("expected attribute structure-array");
        };
        let attr0 = attrs[0].as_ref().expect("attribute present");
        assert_eq!(
            read_ts(attr0, "timeStamp"),
            (0, 0),
            "per-attribute timeStamp must stay default, not the image timestamp"
        );
    }

    /// Regression for the NTNDArray attribute `source` / `sourceType` contract.
    ///
    /// C `NDAttribute` stores the constructor `pSource` verbatim and the
    /// converter publishes it with `pvAttr->getSubField<PVString>("source")
    /// ->put(attr->getSource())` (ntndArrayConverter.cpp:564), while
    /// `sourceType` is the raw `NDAttrSource_t` enum int (NDAttribute.h:62-68:
    /// Driver=0, Param=1, EPICSPV=2, Funct=3, Const=4, Undefined=5).
    /// Driver-created attributes carry the literal `"Driver"`
    /// (NDAttributeList.cpp:73); XML attributes carry their configured `source`
    /// property (PV name, asyn drvInfo, function name, or constant string). The
    /// `source` string must never be synthesized from the type.
    #[test]
    fn attribute_source_string_and_type_match_c() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let mut arr = NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "FromDriver",
            "driver attr",
            NDAttrSource::Driver,
            NDAttrValue::Int32(0),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Counter",
            "param attr",
            NDAttrSource::Param {
                port_name: "SIM1".into(),
                param_name: "ARRAY_COUNTER".into(),
            },
            NDAttrValue::Int32(0),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Temp",
            "epics pv attr",
            NDAttrSource::EpicsPV("13SIM1:cam1:Temperature".into()),
            NDAttrValue::Float64(0.0),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Computed",
            "function attr",
            NDAttrSource::Function("my_func".into()),
            NDAttrValue::Int32(0),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Label",
            "const attr",
            NDAttrSource::Constant("a constant".into()),
            NDAttrValue::String("a constant".into()),
        ));

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };
        let Some(PvField::StructureArray(attrs)) = s.get_field("attribute") else {
            panic!("expected attribute structure-array");
        };
        assert_eq!(attrs.len(), 5);

        let src_str = |i: usize| -> String {
            let a = attrs[i].as_ref().expect("attribute present");
            match a.get_field("source") {
                Some(PvField::Scalar(ScalarValue::String(v))) => v.as_str_lossy().into_owned(),
                other => panic!("expected source string, got {other:?}"),
            }
        };
        let src_type = |i: usize| -> i32 {
            let a = attrs[i].as_ref().expect("attribute present");
            match a.get_field("sourceType") {
                Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
                other => panic!("expected sourceType int, got {other:?}"),
            }
        };

        // Driver: literal "Driver", enum 0 — NOT a lowercase "driver" label.
        assert_eq!(src_str(0), "Driver");
        assert_eq!(src_type(0), 0);
        // Param: the asyn drvInfo verbatim, NOT "{port}.{param}".
        assert_eq!(src_str(1), "ARRAY_COUNTER");
        assert_eq!(src_type(1), 1);
        // EPICS_PV / Function / Const: the configured source string verbatim,
        // NOT the fixed labels "epics" / "function" / "constant".
        assert_eq!(src_str(2), "13SIM1:cam1:Temperature");
        assert_eq!(src_type(2), 2);
        assert_eq!(src_str(3), "my_func");
        assert_eq!(src_type(3), 3);
        assert_eq!(src_str(4), "a constant");
        assert_eq!(src_type(4), 4);
    }

    /// Regression.
    ///
    /// C ADCore fills the two NTNDArray time fields from two distinct sources
    /// (ntndArrayConverter.cpp:477-503): `dataTimeStamp` from the floating-point
    /// `NDArray::timeStamp` (floor the seconds, scale the fraction to ns), and
    /// `timeStamp` from the integer `NDArray::epicsTS`. Both add
    /// POSIX_TIME_AT_EPICS_EPOCH to convert the EPICS epoch to POSIX. Pre-fix
    /// Rust wrote the raw EPICS seconds (no offset) into both fields and sourced
    /// both from `epicsTS`, ignoring the floating-point `timeStamp` entirely.
    #[test]
    fn nt_timestamps_use_distinct_sources_and_posix_offset() {
        use epics_pva_rs::pvdata::PvStructure;

        let mut arr = NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt8);
        // Distinct values so a single-source bug is observable: the
        // floating-point timeStamp carries a sub-second fraction, the integer
        // epicsTS carries different whole seconds and nanoseconds.
        arr.time_stamp = 10.25;
        arr.timestamp.sec = 1000;
        arr.timestamp.nsec = 42;

        let arr = Arc::new(arr);
        let payload = ndarray_to_pv_field(&arr);
        let PvField::Structure(s) = &payload else {
            panic!("expected structure, got {payload:?}");
        };

        let read_ts = |st: &PvStructure, field: &str| -> (i64, i32) {
            let Some(PvField::Structure(ts)) = st.get_field(field) else {
                panic!("expected {field} time_t structure");
            };
            let sec = match ts.get_field("secondsPastEpoch") {
                Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
                other => panic!("expected secondsPastEpoch Long, got {other:?}"),
            };
            let nsec = match ts.get_field("nanoseconds") {
                Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
                other => panic!("expected nanoseconds Int, got {other:?}"),
            };
            (sec, nsec)
        };

        let offset = ad_core_rs::timestamp::EPICS_EPOCH_OFFSET as i64;

        // dataTimeStamp: floor(10.25)=10s + offset, fraction 0.25 -> 250_000_000ns.
        assert_eq!(read_ts(s, "dataTimeStamp"), (10 + offset, 250_000_000));
        // timeStamp: epicsTS 1000s + offset, 42ns — distinct source from above.
        assert_eq!(read_ts(s, "timeStamp"), (1000 + offset, 42));
    }
}
