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
use tokio::sync::mpsc;

use epics_pva_rs::nt::nd_array::{
    NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
    nt_nd_array_value,
};
use epics_pva_rs::pvdata::{PvField, ScalarValue};

/// Latest snapshot + subscriber list, shared with the qsrv adapter via a
/// global registry.
pub type LatestHandle = Arc<Mutex<Option<PvField>>>;
pub type SubscribersHandle = Arc<Mutex<Vec<mpsc::Sender<PvField>>>>;

/// PVA plugin processor: captures the latest NDArray and converts to PvField.
pub struct PvaProcessor {
    pv_name: String,
    latest: LatestHandle,
    subscribers: SubscribersHandle,
}

impl PvaProcessor {
    pub fn new(pv_name: String) -> Self {
        Self {
            pv_name,
            latest: Arc::new(Mutex::new(None)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn latest_handle(&self) -> LatestHandle {
        self.latest.clone()
    }

    pub fn subscribers_handle(&self) -> SubscribersHandle {
        self.subscribers.clone()
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
        *self.latest.lock() = Some(payload.clone());

        // Notify subscribers, remove dead ones.
        let mut subs = self.subscribers.lock();
        subs.retain(|tx| tx.try_send(payload.clone()).is_ok());

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

    let data_time_stamp = epics_ts_to_nt(&array.timestamp);

    let attribute: Vec<NdAttribute> = array
        .attributes
        .iter()
        .map(|a| NdAttribute {
            name: a.name.clone(),
            value: attribute_value_to_scalar(&a.value),
            tags: Vec::new(),
            descriptor: a.description.clone(),
            alarm: NdAlarm::default(),
            time_stamp: data_time_stamp.clone(),
            source_type: ndattr_source_type(&a.source),
            source: ndattr_source_string(&a.source),
        })
        .collect();

    let nt = NtNdArray {
        value,
        codec,
        compressed_size,
        uncompressed_size,
        dimension,
        unique_id: array.unique_id,
        data_time_stamp: data_time_stamp.clone(),
        attribute,
        alarm: NdAlarm {
            severity: 0,
            status: 0,
            message: "NO_ALARM".into(),
        },
        time_stamp: data_time_stamp,
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

fn epics_ts_to_nt(ts: &ad_core_rs::timestamp::EpicsTimestamp) -> NdTimeStamp {
    NdTimeStamp {
        seconds_past_epoch: ts.sec as i64,
        nanoseconds: ts.nsec as i32,
        user_tag: 0,
    }
}

fn codec_name_to_string(name: ad_core_rs::codec::CodecName) -> String {
    name.as_str().to_string()
}

fn ndattr_source_type(src: &ad_core_rs::attributes::NDAttrSource) -> i32 {
    use ad_core_rs::attributes::NDAttrSource;
    match src {
        NDAttrSource::Driver => 0,
        NDAttrSource::Param { .. } => 1,
        NDAttrSource::EpicsPV => 2,
        NDAttrSource::Function => 3,
        NDAttrSource::Constant => 4,
        NDAttrSource::Undefined => -1,
    }
}

fn ndattr_source_string(src: &ad_core_rs::attributes::NDAttrSource) -> String {
    use ad_core_rs::attributes::NDAttrSource;
    match src {
        NDAttrSource::Driver => "driver".into(),
        NDAttrSource::Param {
            port_name,
            param_name,
        } => format!("{port_name}.{param_name}"),
        NDAttrSource::EpicsPV => "epics".into(),
        NDAttrSource::Function => "function".into(),
        NDAttrSource::Constant => "constant".into(),
        NDAttrSource::Undefined => String::new(),
    }
}

fn attribute_value_to_scalar(val: &ad_core_rs::attributes::NDAttrValue) -> ScalarValue {
    use ad_core_rs::attributes::NDAttrValue;
    match val {
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
        NDAttrValue::String(v) => ScalarValue::String(v.clone()),
        NDAttrValue::Undefined => ScalarValue::Int(0),
    }
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

    #[test]
    fn processor_stores_latest() {
        let mut proc = PvaProcessor::new("TEST:Pva1:Image".into());
        let pool = NDArrayPool::new(1_000_000);
        let arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        proc.process_array(&arr, &pool);

        let latest = proc.latest_handle().lock().clone();
        assert!(latest.is_some());
    }
}
