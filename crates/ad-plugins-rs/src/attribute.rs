//! NDPluginAttribute: extracts named attribute values from each array.
//!
//! Supports `maxAttributes` attribute channels (addr 0..maxAttributes-1), each
//! tracking a different attribute by name. Special pseudo-attribute names
//! "NDArrayUniqueId" and "NDArrayTimeStamp" read from the array header.

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    ArraySource, NDPluginProcess, ParamChangeResult, ParamChangeValue, ParamUpdate,
    PluginParamSnapshot, ProcessResult,
};
use asyn_rs::error::AsynError;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use parking_lot::Mutex;
use std::sync::Arc;

/// Parameter indices for NDPluginAttribute.
#[derive(Clone, Copy, Default)]
pub struct AttributeParams {
    pub attr_name: usize,
    pub value: usize,
    pub value_sum: usize,
    pub reset: usize,
}

/// State for a single attribute channel.
#[derive(Clone)]
struct AttrChannel {
    name: String,
    value: f64,
    value_sum: f64,
}

impl Default for AttrChannel {
    fn default() -> Self {
        Self {
            name: String::new(),
            value: 0.0,
            value_sum: 0.0,
        }
    }
}

impl AttrChannel {
    fn extract_value(&self, array: &NDArray) -> Option<f64> {
        if self.name.is_empty() {
            return None;
        }
        match self.name.as_str() {
            "NDArrayUniqueId" => Some(array.unique_id as f64),
            // C `attrValue = pArray->timeStamp` (NDPluginAttribute.cpp:63) — the
            // standalone `double timeStamp`, NOT a value derived from `epicsTS`.
            // A driver with a hardware clock sets the two independently, and C
            // exposes each through its own channel name: `NDArrayTimeStamp` is
            // the double, `NDArrayEpicsTS*` are the epicsTS fields (`:64-67`).
            "NDArrayTimeStamp" => Some(array.time_stamp),
            "NDArrayEpicsTSSec" => Some(array.timestamp.sec as f64),
            "NDArrayEpicsTSnSec" => Some(array.timestamp.nsec as f64),
            _ => array
                .attributes
                .get(&self.name)
                .and_then(|attr| attr.value.as_f64()),
        }
    }
}

/// Processor that extracts multiple attribute values from each array.
pub struct AttributeProcessor {
    channels: Mutex<Vec<AttrChannel>>,
    params: AttributeParams,
}

impl AttributeProcessor {
    /// `num_channels` is C `maxAttributes_` (the per-frame channel count, floored
    /// to >=1; NDPluginAttribute.cpp:184). Channel 0 is seeded with `attr_name`.
    pub fn new(attr_name: &str, num_channels: usize) -> Self {
        let mut channels = vec![AttrChannel::default(); num_channels.max(1)];
        channels[0].name = attr_name.to_string();
        Self {
            channels: Mutex::new(channels),
            params: AttributeParams::default(),
        }
    }

    /// Access the registered param indices (populated after register_params).
    pub fn params(&self) -> &AttributeParams {
        &self.params
    }

    /// Reset value and value_sum for all channels (C parity: resets all, not just one).
    pub fn reset(&self) {
        for ch in self.channels.lock().iter_mut() {
            ch.value = 0.0;
            ch.value_sum = 0.0;
        }
    }

    /// Current extracted value for channel 0.
    pub fn value(&self) -> f64 {
        self.channels.lock()[0].value
    }

    /// Current accumulated sum for channel 0.
    pub fn value_sum(&self) -> f64 {
        self.channels.lock()[0].value_sum
    }

    /// The attribute name being tracked by channel 0.
    pub fn attr_name(&self) -> String {
        self.channels.lock()[0].name.clone()
    }

    /// Set the attribute name for channel 0.
    pub fn set_attr_name(&self, name: &str) {
        self.channels.lock()[0].name = name.to_string();
    }
}

impl NDPluginProcess for AttributeProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
        let mut updates = Vec::new();
        let mut channels = self.channels.lock();

        for (i, ch) in channels.iter_mut().enumerate() {
            if ch.name.is_empty() {
                continue;
            }
            // C `continue`s on a missing or non-numeric attribute: no
            // setDoubleParam, no ValSum accumulation, no callParamCallbacks(i)
            // for that channel (NDPluginAttribute.cpp:72-80). Only post when the
            // value was actually refreshed this frame.
            if let Some(val) = ch.extract_value(array) {
                ch.value = val;
                ch.value_sum += val;
                let addr = i as i32;
                updates.push(ParamUpdate::float64_addr(self.params.value, addr, ch.value));
                updates.push(ParamUpdate::float64_addr(
                    self.params.value_sum,
                    addr,
                    ch.value_sum,
                ));
            }
        }

        // C doTimeSeriesCallbacks posts every channel's value at address 1
        // (NDPluginAttribute.cpp:89, :94-110).
        let values: Vec<f64> = channels.iter().map(|ch| ch.value).collect();
        let mut out = ProcessResult::sink(updates);
        out.addressed.extend(
            crate::time_series_plugin::time_series_point(pool, array, &values).map(|a| (1, a)),
        );
        out
    }

    fn plugin_type(&self) -> &str {
        "NDPluginAttribute"
    }

    /// C `NDPluginAttribute.cpp:203` sets `NDArrayCallbacks = 0`: this plugin
    /// extracts attribute time series and does not deliver arrays downstream.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
        self.params.attr_name = base.create_param("ATTR_ATTRNAME", ParamType::Octet)?;
        self.params.value = base.create_param("ATTR_VAL", ParamType::Float64)?;
        self.params.value_sum = base.create_param("ATTR_VAL_SUM", ParamType::Float64)?;
        self.params.reset = base.create_param("ATTR_RESET", ParamType::Int32)?;
        Ok(())
    }

    fn on_param_change(&self, reason: usize, params: &PluginParamSnapshot) -> ParamChangeResult {
        let addr = params.addr as usize;

        if reason == self.params.attr_name {
            let mut channels = self.channels.lock();
            if addr < channels.len() {
                if let ParamChangeValue::Octet(s) = &params.value {
                    channels[addr].name = s.clone();
                }
            }
        } else if reason == self.params.reset {
            // C zeros Val/ValSum for all channels on ANY write to the reset
            // param — there is no value test (NDPluginAttribute.cpp:123-128).
            let mut updates = Vec::new();
            for (i, ch) in self.channels.lock().iter_mut().enumerate() {
                ch.value = 0.0;
                ch.value_sum = 0.0;
                let a = i as i32;
                updates.push(ParamUpdate::float64_addr(self.params.value, a, 0.0));
                updates.push(ParamUpdate::float64_addr(self.params.value_sum, a, 0.0));
            }
            return ParamChangeResult::updates(updates);
        }

        ParamChangeResult::updates(vec![])
    }
}

/// Create an Attribute plugin runtime; address 1 carries the time-series point
/// an NDPluginTimeSeries port reads.
pub fn create_attribute_runtime(
    claim: &asyn_rs::registry::PortClaim,
    pool: std::sync::Arc<ad_core_rs::ndarray_pool::NDArrayPool>,
    queue_size: usize,
    source: impl Into<ArraySource>,
    wiring: std::sync::Arc<ad_core_rs::plugin::wiring::WiringRegistry>,
    max_attributes: i32,
    threads: ad_core_rs::plugin::runtime::PluginThreads,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    std::thread::JoinHandle<()>,
) {
    // C: maxAttributes_ = max(maxAttributes, 1) is the per-frame channel count
    // and the TS length; the NDPluginDriver base address count is
    // max(maxAttributes, 2) (NDPluginAttribute.cpp:175,184).
    let num_channels = max_attributes.max(1) as usize;
    let num_addr = max_attributes.max(2) as usize;

    let processor = AttributeProcessor::new("", num_channels);

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_multi_addr(
        claim, processor, pool, queue_size, source, wiring, num_addr, threads,
    );

    (handle, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataBuffer, NDDataType, NDDimension};

    fn make_array_with_attr(name: &str, value: f64, uid: i32) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = uid;
        arr.attributes.add(NDAttribute::new_static(
            name,
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::Float64(value),
        ));
        arr
    }

    #[test]
    fn test_extract_named_attribute() {
        let proc = AttributeProcessor::new("Temperature", 8);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attr("Temperature", 25.5, 1);
        let result = proc.process_array(&Arc::new(arr), &pool);

        assert!(
            result.output_arrays.is_empty(),
            "attribute plugin is a sink"
        );
        assert!((proc.value() - 25.5).abs() < 1e-10);
        assert!((proc.value_sum() - 25.5).abs() < 1e-10);
    }

    #[test]
    fn test_sum_accumulation() {
        let proc = AttributeProcessor::new("Intensity", 8);
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = make_array_with_attr("Intensity", 10.0, 1);
        proc.process_array(&Arc::new(arr1), &pool);
        assert!((proc.value_sum() - 10.0).abs() < 1e-10);

        let arr2 = make_array_with_attr("Intensity", 20.0, 2);
        proc.process_array(&Arc::new(arr2), &pool);
        assert!((proc.value() - 20.0).abs() < 1e-10);
        assert!((proc.value_sum() - 30.0).abs() < 1e-10);
    }

    #[test]
    fn test_reset() {
        let proc = AttributeProcessor::new("Count", 8);
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = make_array_with_attr("Count", 100.0, 1);
        proc.process_array(&Arc::new(arr1), &pool);
        assert!((proc.value_sum() - 100.0).abs() < 1e-10);

        proc.reset();
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
        assert!((proc.value() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_special_attr_unique_id() {
        let proc = AttributeProcessor::new("NDArrayUniqueId", 8);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;

        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_special_attr_timestamp() {
        // C `NDPluginAttribute.cpp:63`: the NDArrayTimeStamp channel reads
        // `pArray->timeStamp`. A driver that derives it from epicsTS (the
        // `updateTimeStamps` path) sees the two agree.
        let proc = AttributeProcessor::new("NDArrayTimeStamp", 8);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.update_time_stamps(ad_core_rs::timestamp::EpicsTimestamp {
            sec: 100,
            nsec: 500_000_000,
        });

        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 100.5).abs() < 1e-9);
    }

    #[test]
    fn test_special_attr_timestamp_is_the_standalone_double() {
        // R8-66: `timeStamp` is an independent double, not a view of epicsTS —
        // a driver with a hardware clock sets it on its own (the AD norm). The
        // NDArrayTimeStamp channel must read THAT value (C
        // NDPluginAttribute.cpp:63), while the NDArrayEpicsTS* channels keep
        // reading epicsTS (`:64-67`). The port read epicsTS for all three.
        let pool = NDArrayPool::new(1_000_000);
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.timestamp = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 100,
            nsec: 500_000_000,
        };
        // Hardware time base, deliberately unrelated to epicsTS.
        arr.time_stamp = 7.25;

        let ts = AttributeProcessor::new("NDArrayTimeStamp", 8);
        let arr = Arc::new(arr);
        ts.process_array(&arr, &pool);
        assert!(
            (ts.value() - 7.25).abs() < 1e-9,
            "NDArrayTimeStamp reads pArray->timeStamp, got {}",
            ts.value()
        );

        let sec = AttributeProcessor::new("NDArrayEpicsTSSec", 8);
        sec.process_array(&arr, &pool);
        assert!((sec.value() - 100.0).abs() < 1e-9);

        let nsec = AttributeProcessor::new("NDArrayEpicsTSnSec", 8);
        nsec.process_array(&arr, &pool);
        assert!((nsec.value() - 500_000_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_missing_attribute() {
        let proc = AttributeProcessor::new("NonExistent", 8);
        let pool = NDArrayPool::new(1_000_000);

        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        proc.process_array(&Arc::new(arr), &pool);

        assert!((proc.value() - 0.0).abs() < 1e-10);
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_string_attribute_ignored() {
        let proc = AttributeProcessor::new("Label", 8);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "Label",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::String("hello".to_string()),
        ));

        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_int32_attribute() {
        let proc = AttributeProcessor::new("Counter", 8);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "Counter",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::Int32(7),
        ));

        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_channel_count_follows_max_attributes() {
        // C maxAttributes_ sizes the per-frame channel loop and the TS NDArray
        // length (NDPluginAttribute.cpp:55,98,184); neither is fixed at 8.

        let mut proc = AttributeProcessor::new("Temp", 16);
        proc.params.value = 2;
        proc.params.value_sum = 3;
        proc.channels.lock()[15].name = "High".to_string();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "Temp",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::Float64(1.0),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "High",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::Float64(9.0),
        ));

        let r = proc.process_array(&Arc::new(arr), &NDArrayPool::new(1_000_000));
        // Channel 15 — beyond the old fixed 8 — must post its value.
        assert!(
            r.param_updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Float64 { reason: 2, addr: 15, value } if *value == 9.0
            )),
            "channel 15 must post with a 16-channel processor"
        );
        // The TS point at address 1 has one element per channel.
        let [(addr, ts)] = r.addressed.as_slice() else {
            panic!("one TS point per frame, got {}", r.addressed.len());
        };
        assert_eq!(*addr, 1);
        let NDDataBuffer::F64(v) = &ts.data else {
            panic!("TS point is Float64");
        };
        assert_eq!(v.len(), 16);
        assert_eq!((v[0], v[15]), (1.0, 9.0));
    }

    #[test]
    fn test_missing_attribute_skips_post() {
        // C `continue`s (no setDoubleParam / callParamCallbacks) for a channel
        // whose attribute is absent this frame (NDPluginAttribute.cpp:72-80).
        let mut proc = AttributeProcessor::new("Temp", 8);
        proc.params.value = 2;
        proc.params.value_sum = 3;
        let pool = NDArrayPool::new(1_000_000);

        let r1 = proc.process_array(&Arc::new(make_array_with_attr("Temp", 5.0, 1)), &pool);
        assert!(
            r1.param_updates
                .iter()
                .any(|u| matches!(u, ParamUpdate::Float64 { reason: 2, .. })),
            "present attribute must post ATTR_VAL"
        );

        let bare = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let r2 = proc.process_array(&Arc::new(bare), &pool);
        assert!(
            !r2.param_updates
                .iter()
                .any(|u| matches!(u, ParamUpdate::Float64 { reason: 2, .. })),
            "missing attribute must not re-post stale ATTR_VAL"
        );
        // C retains the last successfully-read Val across the missing frame.
        assert!((proc.value() - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_reset_clears_on_zero_write() {
        // C NDPluginAttribute::writeInt32 zeros Val/ValSum on ANY write to the
        // reset param, including value 0 (NDPluginAttribute.cpp:123-128).
        let mut proc = AttributeProcessor::new("Count", 8);
        proc.params.value = 2;
        proc.params.value_sum = 3;
        proc.params.reset = 7;

        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(make_array_with_attr("Count", 100.0, 1)), &pool);
        assert!((proc.value_sum() - 100.0).abs() < 1e-10);

        let snapshot = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 7,
            addr: 0,
            value: ParamChangeValue::Int32(0),
        };
        let result = proc.on_param_change(7, &snapshot);

        assert!((proc.value() - 0.0).abs() < 1e-10);
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
        assert!(
            result.param_updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Float64 {
                    reason: 2,
                    value,
                    ..
                } if *value == 0.0
            )),
            "zero write must post cleared ATTR_VAL"
        );
    }

    #[test]
    fn test_set_attr_name() {
        let proc = AttributeProcessor::new("A", 8);
        assert_eq!(proc.attr_name(), "A");

        proc.set_attr_name("B");
        assert_eq!(proc.attr_name(), "B");

        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attr("B", 99.0, 1);
        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 99.0).abs() < 1e-10);
    }
}
