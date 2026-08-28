//! NDPluginPva — serves the latest NDArray as an NTNDArray over pvAccess.
//!
//! Corresponds to C++ areaDetector's NDPluginPva. Captures each incoming
//! NDArray, converts it to a native [`PvField::Structure`] shaped per
//! `epics:nt/NTNDArray:1.0`, and stores it in the registry consumed by the
//! qsrv adapter.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use parking_lot::Mutex;

use epics_bridge_rs::qsrv::PvaPvHandle;
use epics_pva_rs::nt::nd_array::{
    NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
    nt_nd_array_desc, nt_nd_array_value,
};
use epics_pva_rs::pvdata::{PvField, ScalarValue, VariantValue};

/// PVA plugin processor: captures the latest NDArray, converts it to an
/// NTNDArray [`PvField`], and posts it through its shared [`PvaPvHandle`].
///
/// The handle is the single validating write owner (pvxs
/// `SharedPV::post`): a frame that does not match the canonical NTNDArray
/// descriptor is rejected and never replaces the last good snapshot or
/// reaches monitor subscribers. The producer holds one clone of the
/// handle and the qsrv adapter registers another; both share the same
/// `latest`/`subscribers` state.
pub struct PvaProcessor {
    pv_name: String,
    handle: PvaPvHandle,
}

impl PvaProcessor {
    pub fn new(pv_name: String) -> Self {
        // The producer always emits NTNDArray-shaped values; advertising
        // `nt_nd_array_desc()` lets `post` gate any malformed frame before
        // it can become the served snapshot.
        Self {
            pv_name,
            handle: PvaPvHandle::new(Some(nt_nd_array_desc())),
        }
    }

    /// A clone of the shared handle to register with the qsrv adapter.
    /// Producer posts and server reads/monitors flow through the same
    /// `latest`/`subscribers` state.
    pub fn handle(&self) -> PvaPvHandle {
        self.handle.clone()
    }
}

impl Default for PvaProcessor {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl NDPluginProcess for PvaProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let payload = ndarray_to_pv_field(array);

        // Regression.
        // `post` validates against the canonical NTNDArray descriptor and
        // is the single owner of `latest`/`subscribers`: a malformed frame
        // is rejected without replacing the last good snapshot or reaching
        // monitor subscribers (pvxs `SharedPV::post`).
        if let Err(err) = self.handle.post(payload) {
            tracing::warn!(
                pv = %self.pv_name,
                error = %err,
                "dropping NDArray frame that does not match the NTNDArray descriptor"
            );
        }

        // Pass through to downstream plugins.
        ProcessResult::arrays(vec![Arc::new(array.clone())])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginPva"
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

fn ndarray_to_pv_field(array: &NDArray) -> PvField {
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
            NdArrayBuffer::UByte(array.data.as_u8_slice().to_vec()),
            c.compressed_size as i64,
            codec_name_to_string(c.name),
        ),
        None => (
            ndbuffer_to_buffer(&array.data),
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

fn ndbuffer_to_buffer(buf: &NDDataBuffer) -> NdArrayBuffer {
    match buf {
        NDDataBuffer::I8(v) => NdArrayBuffer::Byte(v.clone()),
        NDDataBuffer::U8(v) => NdArrayBuffer::UByte(v.clone()),
        NDDataBuffer::I16(v) => NdArrayBuffer::Short(v.clone()),
        NDDataBuffer::U16(v) => NdArrayBuffer::UShort(v.clone()),
        NDDataBuffer::I32(v) => NdArrayBuffer::Int(v.clone()),
        NDDataBuffer::U32(v) => NdArrayBuffer::UInt(v.clone()),
        NDDataBuffer::I64(v) => NdArrayBuffer::Long(v.clone()),
        NDDataBuffer::U64(v) => NdArrayBuffer::ULong(v.clone()),
        NDDataBuffer::F32(v) => NdArrayBuffer::Float(v.clone()),
        NDDataBuffer::F64(v) => NdArrayBuffer::Double(v.clone()),
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

fn codec_name_to_string(name: ad_core_rs::codec::CodecName) -> String {
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
    use epics_pva_rs::pvdata::ScalarType;

    #[test]
    fn convert_simple_array() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 42;
        if let NDDataBuffer::U8(ref mut buf) = arr.data {
            for (i, v) in buf.iter_mut().enumerate() {
                *v = i as u8;
            }
        }
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

    /// Regression: the NTNDArray `dimension[].binning` field must serialize the
    /// raw NDDimension binning verbatim. C ntndArrayConverter.cpp:471 does
    /// `put(src->dims[i].binning)` with no clamp, so a binning of 0 goes out as
    /// 0. Pre-fix Rust applied `.max(1)`, turning a 0 into a wire 1.
    #[test]
    fn dimension_binning_serializes_zero_verbatim() {
        let mut dim = NDDimension::new(8);
        dim.binning = 0;
        let arr = NDArray::new(vec![dim], NDDataType::UInt8);

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
        if let NDDataBuffer::U16(ref mut buf) = arr.data {
            for (i, v) in buf.iter_mut().enumerate() {
                *v = (i * 1000) as u16;
            }
        }
        let uncompressed_bytes = (arr.data.len() * 2) as i64; // 8 elems * 2 bytes

        let compressed = crate::codec::compress_lz4(&arr);
        let comp_size = compressed.codec.as_ref().unwrap().compressed_size as i64;
        assert!(
            matches!(compressed.data, NDDataBuffer::U8(_)),
            "compressed buffer must hold raw bytes"
        );

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
        if let NDDataBuffer::U16(ref mut buf) = arr.data {
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
        let compressed = crate::codec::compress_lz4(&arr);
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

    /// Regression.
    /// A real NDArray frame must match the canonical `nt_nd_array_desc()`
    /// the producer advertises in its handle; otherwise `post` would reject
    /// every frame and the served value would stay empty. Posting a real
    /// frame and asserting `current_value().is_some()` verifies the
    /// producer output and the advertised descriptor agree.
    #[test]
    fn processor_stores_latest() {
        let proc = PvaProcessor::new("TEST:Pva1:Image".into());
        let pool = NDArrayPool::new(1_000_000);
        let arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        proc.process_array(&arr, &pool);

        assert!(proc.handle().current_value().is_some());
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
