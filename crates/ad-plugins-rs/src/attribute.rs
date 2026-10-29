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
use asyn_rs::port::InterfaceMask;
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
    /// to >=1; NDPluginAttribute.cpp:184). Every channel starts with no
    /// attribute name, as NDPluginAttribute.cpp:197 seeds AttrName "".
    pub fn new(num_channels: usize) -> Self {
        let channels = vec![AttrChannel::default(); num_channels.max(1)];
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
    // NDPluginAttribute.cpp:176-177, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::INT32_ARRAY
        .or(InterfaceMask::FLOAT64)
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    // NDPluginAttribute.cpp:136; no writeFloat64 override.
    const WRITE_INT32_EPILOGUE: ad_core_rs::plugin::runtime::WriteEpilogue =
        ad_core_rs::plugin::runtime::WriteEpilogue::Zero;
    /// `std::max<int>(maxAttributes, 2)` (NDPluginAttribute.cpp:175).
    fn max_addr(&self) -> usize {
        self.channels.lock().len().max(2)
    }
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
        // NDPluginAttribute.cpp:194-198 seeds every channel; Reset stays undefined.
        for (i, ch) in self.channels.lock().iter().enumerate() {
            let a = i as i32;
            base.set_float64_param(self.params.value, a, 0.0)?;
            base.set_float64_param(self.params.value_sum, a, 0.0)?;
            base.set_string_param(self.params.attr_name, a, &ch.name)?;
        }
        Ok(())
    }

    /// C `NDPluginAttribute::writeInt32` drops the base refusal (NDPluginAttribute.cpp:136); not copied.
    /// A kept refusal carries the errorMessage C's `if (status)` sets (:138-141).
    /// Reset zeros Val/ValSum on every channel inside the write, whatever the
    /// value (:122-128).
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        _params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        use ad_core_rs::plugin::runtime::{WriteOutcome, WriteVerdict};
        let out = WriteOutcome::from(match verdict {
            WriteVerdict::Refuse { keep, .. } => WriteVerdict::Refuse {
                keep,
                message: format!(
                    "NDPluginAttribute::writeInt32: status=3, function={reason}, value={value}"
                ),
            },
            accepted => accepted,
        });
        if reason != self.params.reset {
            return out;
        }
        let mut channels = self.channels.lock();
        let mut out = out;
        for (i, ch) in channels.iter_mut().enumerate() {
            ch.value = 0.0;
            ch.value_sum = 0.0;
            out = out
                .set_float64(self.params.value, i as i32, 0.0)
                .set_float64(self.params.value_sum, i as i32, 0.0);
        }
        out.settled()
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
        }

        ParamChangeResult::updates(vec![])
    }
}

/// Create an Attribute plugin runtime; address 1 carries the time-series point
/// an NDPluginTimeSeries port reads.
pub fn create_attribute_runtime(
    claim: &asyn_rs::registry::PortClaim,
    max_memory: usize,
    queue_size: i32,
    blocking_callbacks: i32,
    source: impl Into<ArraySource>,
    wiring: std::sync::Arc<ad_core_rs::plugin::wiring::WiringRegistry>,
    max_attributes: i32,
    threads: ad_core_rs::plugin::runtime::PluginThreads,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    std::thread::JoinHandle<()>,
) {
    // C: maxAttributes_ = max(maxAttributes, 1) is the per-frame channel count
    // and the TS length (NDPluginAttribute.cpp:184).
    let processor = AttributeProcessor::new(max_attributes.max(1) as usize);

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        claim,
        processor,
        max_memory,
        queue_size,
        blocking_callbacks,
        source,
        wiring,
        threads,
    );

    (handle, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NDPluginAttribute.cpp:175 passes max(maxAttributes, 2) whoever builds
    /// the plugin.
    #[test]
    fn an_attribute_port_has_c_s_addresses() {
        for (n, addrs) in [(1, 2), (3, 3)] {
            let (handle, _jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
                &asyn_rs::asyn_record::claim_port(&format!("ATTR_ADDR{n}")).unwrap(),
                AttributeProcessor::new(n),
                1_000_000,
                1,
                0,
                "",
                std::sync::Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
                ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
            );
            assert_eq!(handle.port_runtime().port_handle().max_addr(), addrs);
        }
    }

    /// NDPluginAttribute.cpp:136 drops the base refusal; an Attribute port
    /// returns it.
    #[test]
    fn a_refused_base_write_fails() {
        let (message, function, value) =
            crate::test_util::refused_base_write("ATTRIBUTE_REFUSED", AttributeProcessor::new(8));
        assert_eq!(
            message,
            format!("NDPluginAttribute::writeInt32: status=3, function={function}, value={value}")
        );
    }
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataType, NDDataView, NDDimension};

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
    fn a_write_at_another_address_stores_at_address_zero() {
        // NDPluginAttribute.cpp:121: setIntegerParam(function, value) is
        // list 0 whatever address was written.
        use asyn_rs::port::DrvUserRequest;
        let (handle, _jh) = create_attribute_runtime(
            &asyn_rs::asyn_record::claim_port("ATTR_RST1").unwrap(),
            1_000_000,
            1,
            0,
            "",
            Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
            3,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let reset = port
            .drv_user_create_blocking(&DrvUserRequest::new("ATTR_RESET", 0))
            .unwrap()
            .reason;
        port.write_int32_blocking(reset, 1, 1).unwrap();
        assert_eq!(port.read_int32_blocking(reset, 0).unwrap(), 1);
        assert!(port.read_int32_blocking(reset, 1).is_err());
    }

    #[test]
    fn test_register_params_seeds_every_channel() {
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "ATTR_INIT",
            3,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let mut proc = AttributeProcessor::new(3);
        proc.register_params(&mut base).unwrap();
        let p = *proc.params();
        for a in 0..3 {
            assert_eq!(base.get_float64_param(p.value, a).unwrap(), 0.0);
            assert_eq!(base.get_float64_param(p.value_sum, a).unwrap(), 0.0);
            assert_eq!(base.get_string_param(p.attr_name, a).unwrap(), b"");
        }
        assert!(base.get_int32_param(p.reset, 0).is_err());
    }

    #[test]
    fn test_extract_named_attribute() {
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("Temperature");
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
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("Intensity");
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = make_array_with_attr("Intensity", 10.0, 1);
        proc.process_array(&Arc::new(arr1), &pool);
        assert!((proc.value_sum() - 10.0).abs() < 1e-10);

        let arr2 = make_array_with_attr("Intensity", 20.0, 2);
        proc.process_array(&Arc::new(arr2), &pool);
        assert!((proc.value() - 20.0).abs() < 1e-10);
        assert!((proc.value_sum() - 30.0).abs() < 1e-10);
    }

    /// NDPluginAttribute.cpp:122-128: a Reset write of any value zeros every
    /// channel's Val/ValSum inside the write.
    #[test]
    fn reset_zeros_every_channel_inside_the_write_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::{SideParam, WriteVerdict};
        use asyn_rs::port::InterfaceMask;
        let mut proc = AttributeProcessor::new(2);
        proc.set_attr_name("Count");
        let mut base = PortDriverBase::new(
            "T",
            2,
            Default::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        proc.register_params(&mut base).unwrap();
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(make_array_with_attr("Count", 100.0, 1)), &pool);

        let p = proc.params;
        let out = proc.validate_int32(
            p.reset,
            0,
            0,
            WriteVerdict::Accept(0),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled);
        let zero = |reason, addr| SideParam::Float64 {
            reason,
            addr,
            value: 0.0,
        };
        assert_eq!(
            out.side,
            [
                zero(p.value, 0),
                zero(p.value_sum, 0),
                zero(p.value, 1),
                zero(p.value_sum, 1)
            ]
        );
        assert_eq!(proc.value_sum(), 0.0);
    }

    #[test]
    fn test_reset() {
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("Count");
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
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("NDArrayUniqueId");
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
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("NDArrayTimeStamp");
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

        let ts = AttributeProcessor::new(8);

        ts.set_attr_name("NDArrayTimeStamp");
        let arr = Arc::new(arr);
        ts.process_array(&arr, &pool);
        assert!(
            (ts.value() - 7.25).abs() < 1e-9,
            "NDArrayTimeStamp reads pArray->timeStamp, got {}",
            ts.value()
        );

        let sec = AttributeProcessor::new(8);

        sec.set_attr_name("NDArrayEpicsTSSec");
        sec.process_array(&arr, &pool);
        assert!((sec.value() - 100.0).abs() < 1e-9);

        let nsec = AttributeProcessor::new(8);

        nsec.set_attr_name("NDArrayEpicsTSnSec");
        nsec.process_array(&arr, &pool);
        assert!((nsec.value() - 500_000_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_missing_attribute() {
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("NonExistent");
        let pool = NDArrayPool::new(1_000_000);

        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        proc.process_array(&Arc::new(arr), &pool);

        assert!((proc.value() - 0.0).abs() < 1e-10);
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_string_attribute_ignored() {
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("Label");
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
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("Counter");
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

        let mut proc = AttributeProcessor::new(16);

        proc.set_attr_name("Temp");
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
        let NDDataView::F64(v) = ts.data().view() else {
            panic!("TS point is Float64");
        };
        assert_eq!(v.len(), 16);
        assert_eq!((v[0], v[15]), (1.0, 9.0));
    }

    #[test]
    fn test_missing_attribute_skips_post() {
        // C `continue`s (no setDoubleParam / callParamCallbacks) for a channel
        // whose attribute is absent this frame (NDPluginAttribute.cpp:72-80).
        let mut proc = AttributeProcessor::new(8);
        proc.set_attr_name("Temp");
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
    fn test_set_attr_name() {
        let proc = AttributeProcessor::new(8);
        proc.set_attr_name("A");
        assert_eq!(proc.attr_name(), "A");

        proc.set_attr_name("B");
        assert_eq!(proc.attr_name(), "B");

        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attr("B", 99.0, 1);
        proc.process_array(&Arc::new(arr), &pool);
        assert!((proc.value() - 99.0).abs() < 1e-10);
    }
}
