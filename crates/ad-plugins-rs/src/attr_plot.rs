//! NDPluginAttrPlot: tracks numeric attribute values over time in circular buffers.
//!
//! On the first frame, the plugin scans the array's attribute list and auto-detects
//! all numeric attributes (those where `as_f64()` returns `Some`). The attribute names
//! are sorted alphabetically for deterministic ordering. On subsequent frames, each
//! tracked attribute's value is pushed into a per-attribute circular buffer (VecDeque).
//!
//! If the array's `unique_id` decreases relative to the previous frame, all buffers
//! are reset (indicating a new acquisition).

use std::collections::VecDeque;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Processor that tracks attribute values over time in circular buffers.
pub struct AttrPlotProcessor {
    /// Tracked attribute names (sorted alphabetically).
    attributes: Vec<String>,
    /// Per-attribute circular buffer of values.
    buffers: Vec<VecDeque<f64>>,
    /// Circular buffer of unique_id values.
    uid_buffer: VecDeque<f64>,
    /// Maximum number of points per buffer. 0 = unlimited.
    max_points: usize,
    /// Whether we have initialized from the first frame.
    initialized: bool,
    /// The unique_id from the last processed frame.
    last_uid: i32,
}

impl AttrPlotProcessor {
    /// Create a new processor with the given maximum buffer size.
    pub fn new(max_points: usize) -> Self {
        Self {
            attributes: Vec::new(),
            buffers: Vec::new(),
            uid_buffer: VecDeque::new(),
            max_points,
            initialized: false,
            last_uid: -1,
        }
    }

    /// Get the list of tracked attribute names.
    pub fn attributes(&self) -> &[String] {
        &self.attributes
    }

    /// Get the circular buffer for a specific attribute index.
    pub fn buffer(&self, index: usize) -> Option<&VecDeque<f64>> {
        self.buffers.get(index)
    }

    /// Get the unique_id buffer.
    pub fn uid_buffer(&self) -> &VecDeque<f64> {
        &self.uid_buffer
    }

    /// Get the number of tracked attributes.
    pub fn num_attributes(&self) -> usize {
        self.attributes.len()
    }

    /// Find the index of a named attribute. Returns None if not tracked.
    pub fn find_attribute(&self, name: &str) -> Option<usize> {
        self.attributes.iter().position(|n| n == name)
    }

    /// Reset all buffers and re-initialize on the next frame.
    pub fn reset(&mut self) {
        self.attributes.clear();
        self.buffers.clear();
        self.uid_buffer.clear();
        self.initialized = false;
        self.last_uid = -1;
    }

    /// Push a value to a VecDeque, enforcing max_points as a ring buffer.
    fn push_capped(buf: &mut VecDeque<f64>, value: f64, max_points: usize) {
        if max_points > 0 && buf.len() >= max_points {
            buf.pop_front();
        }
        buf.push_back(value);
    }

    /// Initialize tracked attributes from the first frame.
    fn initialize_from_array(&mut self, array: &NDArray) {
        let mut names: Vec<String> = Vec::new();
        for attr in array.attributes.iter() {
            if attr.value.as_f64().is_some() {
                names.push(attr.name.clone());
            }
        }
        names.sort();

        self.buffers = vec![VecDeque::new(); names.len()];
        self.attributes = names;
        self.uid_buffer.clear();
        self.initialized = true;
    }

    /// Clear all data buffers but keep tracked attributes.
    fn clear_buffers(&mut self) {
        for buf in &mut self.buffers {
            buf.clear();
        }
        self.uid_buffer.clear();
    }
}

impl NDPluginProcess for AttrPlotProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // Detect UID decrease (re-acquisition)
        if self.initialized && array.unique_id < self.last_uid {
            self.clear_buffers();
        }
        self.last_uid = array.unique_id;

        // Initialize on first frame
        if !self.initialized {
            self.initialize_from_array(array);
        }

        // Push unique_id
        Self::push_capped(
            &mut self.uid_buffer,
            array.unique_id as f64,
            self.max_points,
        );

        // Push each tracked attribute's value
        for (i, name) in self.attributes.iter().enumerate() {
            let value = array
                .attributes
                .get(name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or(0.0);
            Self::push_capped(&mut self.buffers[i], value, self.max_points);
        }

        ProcessResult::sink(vec![])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginAttrPlot"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_array_with_attrs(uid: i32, attrs: &[(&str, f64)]) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = uid;
        for (name, value) in attrs {
            arr.attributes.add(NDAttribute {
                name: name.to_string(),
                description: String::new(),
                source: NDAttrSource::Driver,
                value: NDAttrValue::Float64(*value),
            });
        }
        arr
    }

    #[test]
    fn test_attribute_auto_detection() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = make_array_with_attrs(1, &[("Temp", 25.0), ("Gain", 1.5)]);
        // Add a string attribute that should be excluded
        arr.attributes.add(NDAttribute {
            name: "Label".to_string(),
            description: String::new(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::String("test".to_string()),
        });

        proc.process_array(&arr, &pool);

        // Should detect 2 numeric attributes, sorted
        assert_eq!(proc.num_attributes(), 2);
        assert_eq!(proc.attributes()[0], "Gain");
        assert_eq!(proc.attributes()[1], "Temp");
    }

    #[test]
    fn test_value_tracking() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        for i in 0..5 {
            let arr = make_array_with_attrs(i, &[("Value", i as f64 * 10.0)]);
            proc.process_array(&arr, &pool);
        }

        let idx = proc.find_attribute("Value").unwrap();
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 5);
        assert!((buf[0] - 0.0).abs() < 1e-10);
        assert!((buf[4] - 40.0).abs() < 1e-10);
    }

    #[test]
    fn test_uid_buffer() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        for i in 1..=3 {
            let arr = make_array_with_attrs(i, &[("X", 1.0)]);
            proc.process_array(&arr, &pool);
        }

        let uid_buf = proc.uid_buffer();
        assert_eq!(uid_buf.len(), 3);
        assert!((uid_buf[0] - 1.0).abs() < 1e-10);
        assert!((uid_buf[1] - 2.0).abs() < 1e-10);
        assert!((uid_buf[2] - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_circular_buffer_max_points() {
        let mut proc = AttrPlotProcessor::new(3);
        let pool = NDArrayPool::new(1_000_000);

        for i in 0..5 {
            let arr = make_array_with_attrs(i, &[("Val", i as f64)]);
            proc.process_array(&arr, &pool);
        }

        let idx = proc.find_attribute("Val").unwrap();
        let buf = proc.buffer(idx).unwrap();
        // Only last 3 values should remain
        assert_eq!(buf.len(), 3);
        assert!((buf[0] - 2.0).abs() < 1e-10);
        assert!((buf[1] - 3.0).abs() < 1e-10);
        assert!((buf[2] - 4.0).abs() < 1e-10);

        // UID buffer also limited
        assert_eq!(proc.uid_buffer().len(), 3);
    }

    #[test]
    fn test_uid_decrease_resets_buffers() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        // First acquisition
        for i in 1..=5 {
            let arr = make_array_with_attrs(i, &[("X", i as f64)]);
            proc.process_array(&arr, &pool);
        }

        let idx = proc.find_attribute("X").unwrap();
        assert_eq!(proc.buffer(idx).unwrap().len(), 5);

        // New acquisition: UID resets to 1
        let arr = make_array_with_attrs(1, &[("X", 100.0)]);
        proc.process_array(&arr, &pool);

        // Buffers should be cleared and have just the new point
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 1);
        assert!((buf[0] - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_missing_attribute_uses_zero() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        // Frame 1: has attribute
        let arr1 = make_array_with_attrs(1, &[("Temp", 25.0)]);
        proc.process_array(&arr1, &pool);

        // Frame 2: attribute missing
        let arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let mut arr2 = arr2;
        arr2.unique_id = 2;
        proc.process_array(&arr2, &pool);

        let idx = proc.find_attribute("Temp").unwrap();
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 2);
        assert!((buf[0] - 25.0).abs() < 1e-10);
        assert!((buf[1] - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_manual_reset() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attrs(1, &[("A", 1.0), ("B", 2.0)]);
        proc.process_array(&arr, &pool);
        assert_eq!(proc.num_attributes(), 2);

        proc.reset();
        assert_eq!(proc.num_attributes(), 0);
        assert!(proc.uid_buffer().is_empty());

        // Re-initializes from next frame
        let arr2 = make_array_with_attrs(1, &[("C", 3.0)]);
        proc.process_array(&arr2, &pool);
        assert_eq!(proc.num_attributes(), 1);
        assert_eq!(proc.attributes()[0], "C");
    }

    #[test]
    fn test_unlimited_buffer() {
        let mut proc = AttrPlotProcessor::new(0);
        let pool = NDArrayPool::new(1_000_000);

        for i in 0..100 {
            let arr = make_array_with_attrs(i, &[("X", i as f64)]);
            proc.process_array(&arr, &pool);
        }

        let idx = proc.find_attribute("X").unwrap();
        assert_eq!(proc.buffer(idx).unwrap().len(), 100);
    }

    #[test]
    fn test_multiple_attributes_sorted() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attrs(1, &[("Zebra", 1.0), ("Alpha", 2.0), ("Mid", 3.0)]);
        proc.process_array(&arr, &pool);

        assert_eq!(proc.attributes(), &["Alpha", "Mid", "Zebra"]);
    }

    #[test]
    fn test_find_attribute() {
        let mut proc = AttrPlotProcessor::new(100);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attrs(1, &[("X", 1.0), ("Y", 2.0)]);
        proc.process_array(&arr, &pool);

        assert_eq!(proc.find_attribute("X"), Some(0));
        assert_eq!(proc.find_attribute("Y"), Some(1));
        assert_eq!(proc.find_attribute("Z"), None);
    }

    #[test]
    fn test_plugin_type() {
        let proc = AttrPlotProcessor::new(100);
        assert_eq!(proc.plugin_type(), "NDPluginAttrPlot");
    }
}
