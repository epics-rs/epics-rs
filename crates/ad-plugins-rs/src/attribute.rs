//! NDPluginAttribute: extracts a single named attribute value from each array.
//!
//! This plugin reads an attribute by name from incoming arrays and tracks
//! the value and cumulative sum. It supports special pseudo-attribute names
//! "NDArrayUniqueId" and "NDArrayTimeStamp" that read from the array header
//! instead of the attribute list.

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};
use asyn_rs::error::AsynError;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

/// Parameter indices for NDPluginAttribute.
#[derive(Clone, Copy, Default)]
pub struct AttributeParams {
    pub attr_name: usize,
    pub value: usize,
    pub value_sum: usize,
    pub reset: usize,
}

/// Processor that extracts a single attribute value from each array.
pub struct AttributeProcessor {
    attr_name: String,
    value: f64,
    value_sum: f64,
    params: AttributeParams,
}

impl AttributeProcessor {
    pub fn new(attr_name: &str) -> Self {
        Self {
            attr_name: attr_name.to_string(),
            value: 0.0,
            value_sum: 0.0,
            params: AttributeParams::default(),
        }
    }

    /// Reset the accumulated sum to zero.
    pub fn reset(&mut self) {
        self.value_sum = 0.0;
    }

    /// Current extracted value.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Current accumulated sum.
    pub fn value_sum(&self) -> f64 {
        self.value_sum
    }

    /// The attribute name being tracked.
    pub fn attr_name(&self) -> &str {
        &self.attr_name
    }

    /// Set the attribute name to track.
    pub fn set_attr_name(&mut self, name: &str) {
        self.attr_name = name.to_string();
    }

    /// Extract the value for the configured attribute from an array.
    fn extract_value(&self, array: &NDArray) -> Option<f64> {
        match self.attr_name.as_str() {
            "NDArrayUniqueId" => Some(array.unique_id as f64),
            "NDArrayTimeStamp" => Some(array.timestamp.as_f64()),
            _ => array
                .attributes
                .get(&self.attr_name)
                .and_then(|attr| attr.value.as_f64()),
        }
    }
}

impl NDPluginProcess for AttributeProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        if let Some(val) = self.extract_value(array) {
            self.value = val;
            self.value_sum += val;
        }

        let updates = vec![
            ParamUpdate::float64(self.params.value, self.value),
            ParamUpdate::float64(self.params.value_sum, self.value_sum),
        ];

        ProcessResult::sink(updates)
    }

    fn plugin_type(&self) -> &str {
        "NDPluginAttribute"
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
        self.params.attr_name = base.create_param("ATTR_NAME", ParamType::Octet)?;
        self.params.value = base.create_param("ATTR_VAL", ParamType::Float64)?;
        self.params.value_sum = base.create_param("ATTR_VAL_SUM", ParamType::Float64)?;
        self.params.reset = base.create_param("ATTR_RESET", ParamType::Int32)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_array_with_attr(name: &str, value: f64, uid: i32) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = uid;
        arr.attributes.add(NDAttribute {
            name: name.to_string(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(value),
        });
        arr
    }

    #[test]
    fn test_extract_named_attribute() {
        let mut proc = AttributeProcessor::new("Temperature");
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attr("Temperature", 25.5, 1);
        let result = proc.process_array(&arr, &pool);

        assert!(
            result.output_arrays.is_empty(),
            "attribute plugin is a sink"
        );
        assert!((proc.value() - 25.5).abs() < 1e-10);
        assert!((proc.value_sum() - 25.5).abs() < 1e-10);
    }

    #[test]
    fn test_sum_accumulation() {
        let mut proc = AttributeProcessor::new("Intensity");
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = make_array_with_attr("Intensity", 10.0, 1);
        proc.process_array(&arr1, &pool);
        assert!((proc.value_sum() - 10.0).abs() < 1e-10);

        let arr2 = make_array_with_attr("Intensity", 20.0, 2);
        proc.process_array(&arr2, &pool);
        assert!((proc.value() - 20.0).abs() < 1e-10);
        assert!((proc.value_sum() - 30.0).abs() < 1e-10);

        let arr3 = make_array_with_attr("Intensity", 5.0, 3);
        proc.process_array(&arr3, &pool);
        assert!((proc.value() - 5.0).abs() < 1e-10);
        assert!((proc.value_sum() - 35.0).abs() < 1e-10);
    }

    #[test]
    fn test_reset() {
        let mut proc = AttributeProcessor::new("Count");
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = make_array_with_attr("Count", 100.0, 1);
        proc.process_array(&arr1, &pool);
        assert!((proc.value_sum() - 100.0).abs() < 1e-10);

        proc.reset();
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
        // value retains last reading
        assert!((proc.value() - 100.0).abs() < 1e-10);

        let arr2 = make_array_with_attr("Count", 50.0, 2);
        proc.process_array(&arr2, &pool);
        assert!((proc.value_sum() - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_special_attr_unique_id() {
        let mut proc = AttributeProcessor::new("NDArrayUniqueId");
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;

        proc.process_array(&arr, &pool);
        assert!((proc.value() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_special_attr_timestamp() {
        let mut proc = AttributeProcessor::new("NDArrayTimeStamp");
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.timestamp = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 100,
            nsec: 500_000_000,
        };

        proc.process_array(&arr, &pool);
        assert!((proc.value() - 100.5).abs() < 1e-9);
    }

    #[test]
    fn test_missing_attribute() {
        let mut proc = AttributeProcessor::new("NonExistent");
        let pool = NDArrayPool::new(1_000_000);

        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        proc.process_array(&arr, &pool);

        // value remains at default (0.0) when attribute is not found
        assert!((proc.value() - 0.0).abs() < 1e-10);
        assert!((proc.value_sum() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_string_attribute_ignored() {
        let mut proc = AttributeProcessor::new("Label");
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute {
            name: "Label".to_string(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::String("hello".to_string()),
        });

        proc.process_array(&arr, &pool);
        // String attrs return None from as_f64(), so value stays 0.0
        assert!((proc.value() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_int32_attribute() {
        let mut proc = AttributeProcessor::new("Counter");
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute {
            name: "Counter".to_string(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(7),
        });

        proc.process_array(&arr, &pool);
        assert!((proc.value() - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_set_attr_name() {
        let mut proc = AttributeProcessor::new("A");
        assert_eq!(proc.attr_name(), "A");

        proc.set_attr_name("B");
        assert_eq!(proc.attr_name(), "B");

        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attr("B", 99.0, 1);
        proc.process_array(&arr, &pool);
        assert!((proc.value() - 99.0).abs() < 1e-10);
    }
}
