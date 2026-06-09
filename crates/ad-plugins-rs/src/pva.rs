//! NDPluginPva — serves the latest NDArray as an NTNDArray over pvAccess.
//!
//! Corresponds to C++ areaDetector's NDPluginPva. Captures each incoming
//! NDArray, converts it to a native [`PvField::Structure`] shaped per
//! `epics:nt/NTNDArray:1.0`, and stores it in the registry consumed by the
//! qsrv adapter.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let payload = ndarray_to_pv_field(array);

        // Regression R0604-BRQSRV-NATIVE-PVA-BAD-POST-CLEARS-VALUE-1.
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
    let value = ndbuffer_to_buffer(&array.data);
    let element_size = array.data.data_type().element_size() as i64;
    let num_elements = array.data.len() as i64;
    let uncompressed_size = num_elements * element_size;

    let (compressed_size, codec) = match &array.codec {
        Some(c) => (
            c.compressed_size as i64,
            NdCodec {
                name: codec_name_to_string(c.name),
                parameters: None,
            },
        ),
        None => (
            uncompressed_size,
            NdCodec {
                name: String::new(),
                parameters: None,
            },
        ),
    };

    let dimension: Vec<NdDimension> = array
        .dims
        .iter()
        .map(|d| NdDimension {
            size: d.size as i32,
            offset: d.offset as i32,
            full_size: d.size as i32,
            binning: d.binning.max(1) as i32,
            reverse: d.reverse,
        })
        .collect();

    // C ADCore fills the two NTNDArray time fields from two distinct sources
    // (ntndArrayConverter.cpp:477-501): `dataTimeStamp` from the floating-point
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

    /// Regression R0604-BRQSRV-NATIVE-PVA-BAD-POST-CLEARS-VALUE-1.
    /// A real NDArray frame must match the canonical `nt_nd_array_desc()`
    /// the producer advertises in its handle; otherwise `post` would reject
    /// every frame and the served value would stay empty. Posting a real
    /// frame and asserting `current_value().is_some()` verifies the
    /// producer output and the advertised descriptor agree.
    #[test]
    fn processor_stores_latest() {
        let mut proc = PvaProcessor::new("TEST:Pva1:Image".into());
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

    /// Regression R0604-ADPVA-ATTR-TIMESTAMP-1.
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
        // offset added (see R0604-ADPVA-TIMESTAMP-1): dataTimeStamp from the
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

    /// Regression R0604-ADPVA-TIMESTAMP-1.
    ///
    /// C ADCore fills the two NTNDArray time fields from two distinct sources
    /// (ntndArrayConverter.cpp:477-501): `dataTimeStamp` from the floating-point
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
