use std::collections::VecDeque;
use std::sync::Arc;

use ad_core::ndarray::NDArray;
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Trigger condition for circular buffer.
#[derive(Debug, Clone)]
pub enum TriggerCondition {
    /// Trigger on an attribute value exceeding threshold.
    AttributeThreshold { name: String, threshold: f64 },
    /// External trigger (manual).
    External,
}

/// Circular buffer state for pre/post-trigger capture.
pub struct CircularBuffer {
    pre_count: usize,
    post_count: usize,
    buffer: VecDeque<Arc<NDArray>>,
    trigger_condition: TriggerCondition,
    triggered: bool,
    post_remaining: usize,
    captured: Vec<Arc<NDArray>>,
}

impl CircularBuffer {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            pre_count,
            post_count,
            buffer: VecDeque::with_capacity(pre_count + 1),
            trigger_condition: condition,
            triggered: false,
            post_remaining: 0,
            captured: Vec::new(),
        }
    }

    /// Push an array into the circular buffer.
    /// Returns true if a complete capture sequence is ready.
    pub fn push(&mut self, array: Arc<NDArray>) -> bool {
        if self.triggered {
            // Post-trigger capture
            self.captured.push(array);
            self.post_remaining -= 1;
            if self.post_remaining == 0 {
                self.triggered = false;
                return true;
            }
            return false;
        }

        // Check trigger condition
        let trigger = match &self.trigger_condition {
            TriggerCondition::AttributeThreshold { name, threshold } => {
                array.attributes.get(name)
                    .and_then(|a| a.value.as_f64())
                    .map(|v| v >= *threshold)
                    .unwrap_or(false)
            }
            TriggerCondition::External => false,
        };

        // Maintain pre-trigger ring buffer
        self.buffer.push_back(array);
        if self.buffer.len() > self.pre_count {
            self.buffer.pop_front();
        }

        if trigger {
            self.trigger();
        }

        false
    }

    /// External trigger.
    pub fn trigger(&mut self) {
        self.triggered = true;
        self.post_remaining = self.post_count;
        // Flush pre-trigger buffer to captured
        self.captured.clear();
        self.captured.extend(self.buffer.drain(..));
    }

    /// Take the captured arrays (pre + post trigger).
    pub fn take_captured(&mut self) -> Vec<Arc<NDArray>> {
        std::mem::take(&mut self.captured)
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered
    }

    pub fn pre_buffer_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.captured.clear();
        self.triggered = false;
        self.post_remaining = 0;
    }
}

// --- New CircularBuffProcessor (NDPluginProcess-based) ---

/// CircularBuff processor: maintains ring buffer state, emits captured arrays on trigger.
pub struct CircularBuffProcessor {
    buffer: CircularBuffer,
}

impl CircularBuffProcessor {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            buffer: CircularBuffer::new(pre_count, post_count, condition),
        }
    }

    pub fn trigger(&mut self) {
        self.buffer.trigger();
    }

    pub fn buffer(&self) -> &CircularBuffer {
        &self.buffer
    }
}

impl NDPluginProcess for CircularBuffProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let done = self.buffer.push(Arc::new(array.clone()));
        if done {
            self.buffer.take_captured()
        } else {
            vec![]
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCircularBuff"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};
    use ad_core::attributes::{NDAttribute, NDAttrSource, NDAttrValue};

    fn make_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    fn make_array_with_attr(id: i32, attr_val: f64) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr.attributes.add(NDAttribute {
            name: "trigger".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(attr_val),
        });
        Arc::new(arr)
    }

    #[test]
    fn test_pre_trigger_buffering() {
        let mut cb = CircularBuffer::new(3, 2, TriggerCondition::External);

        for i in 0..5 {
            cb.push(make_array(i));
        }
        // Pre-buffer should hold last 3
        assert_eq!(cb.pre_buffer_len(), 3);
    }

    #[test]
    fn test_external_trigger() {
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::External);

        cb.push(make_array(1));
        cb.push(make_array(2));
        cb.push(make_array(3));
        // Pre-buffer: [2, 3]

        cb.trigger();
        assert!(cb.is_triggered());

        cb.push(make_array(4));
        let done = cb.push(make_array(5));
        assert!(done);

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 4); // 2 pre + 2 post
        assert_eq!(captured[0].unique_id, 2);
        assert_eq!(captured[1].unique_id, 3);
        assert_eq!(captured[2].unique_id, 4);
        assert_eq!(captured[3].unique_id, 5);
    }

    #[test]
    fn test_attribute_trigger() {
        let mut cb = CircularBuffer::new(1, 1, TriggerCondition::AttributeThreshold {
            name: "trigger".into(),
            threshold: 5.0,
        });

        cb.push(make_array_with_attr(1, 1.0));
        cb.push(make_array_with_attr(2, 2.0));
        assert!(!cb.is_triggered());

        // This should trigger (attr >= 5.0)
        cb.push(make_array_with_attr(3, 5.0));
        assert!(cb.is_triggered());

        let done = cb.push(make_array(4));
        assert!(done);

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 2); // 1 pre + 1 post
    }
}
