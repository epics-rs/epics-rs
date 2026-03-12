use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use ad_core::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core::ndarray::NDArray;
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};
use serde::Deserialize;

/// Position mode: Discard consumes positions, Keep cycles through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosMode {
    Discard,
    Keep,
}

/// JSON-deserializable position list.
#[derive(Debug, Deserialize)]
pub struct PositionList {
    pub positions: Vec<HashMap<String, f64>>,
}

/// NDPosPlugin processor: attaches position metadata to arrays from a position list.
pub struct PosPluginProcessor {
    positions: VecDeque<HashMap<String, f64>>,
    all_positions: Vec<HashMap<String, f64>>,
    mode: PosMode,
    index: usize,
    running: bool,
    expected_id: i32,
    missing_frames: usize,
    duplicate_frames: usize,
}

impl PosPluginProcessor {
    pub fn new(mode: PosMode) -> Self {
        Self {
            positions: VecDeque::new(),
            all_positions: Vec::new(),
            mode,
            index: 0,
            running: false,
            expected_id: 0,
            missing_frames: 0,
            duplicate_frames: 0,
        }
    }

    /// Load positions from a JSON string.
    pub fn load_positions_json(&mut self, json_str: &str) -> Result<usize, serde_json::Error> {
        let list: PositionList = serde_json::from_str(json_str)?;
        let count = list.positions.len();
        self.all_positions = list.positions.clone();
        self.positions = list.positions.into();
        self.index = 0;
        Ok(count)
    }

    /// Load positions directly.
    pub fn load_positions(&mut self, positions: Vec<HashMap<String, f64>>) {
        self.all_positions = positions.clone();
        self.positions = positions.into();
        self.index = 0;
    }

    /// Start processing.
    pub fn start(&mut self) {
        self.running = true;
        self.expected_id = 0;
        self.missing_frames = 0;
        self.duplicate_frames = 0;
    }

    /// Stop processing.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Clear all positions.
    pub fn clear(&mut self) {
        self.positions.clear();
        self.all_positions.clear();
        self.index = 0;
    }

    pub fn missing_frames(&self) -> usize {
        self.missing_frames
    }

    pub fn duplicate_frames(&self) -> usize {
        self.duplicate_frames
    }

    pub fn remaining_positions(&self) -> usize {
        match self.mode {
            PosMode::Discard => self.positions.len(),
            PosMode::Keep => self.all_positions.len(),
        }
    }

    fn current_position(&self) -> Option<&HashMap<String, f64>> {
        match self.mode {
            PosMode::Discard => self.positions.front(),
            PosMode::Keep => {
                if self.all_positions.is_empty() {
                    None
                } else {
                    Some(&self.all_positions[self.index % self.all_positions.len()])
                }
            }
        }
    }

    fn advance(&mut self) {
        match self.mode {
            PosMode::Discard => { self.positions.pop_front(); }
            PosMode::Keep => { self.index += 1; }
        }
    }
}

impl NDPluginProcess for PosPluginProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        if !self.running {
            return ProcessResult::arrays(vec![Arc::new(array.clone())]);
        }

        let has_positions = match self.mode {
            PosMode::Discard => !self.positions.is_empty(),
            PosMode::Keep => !self.all_positions.is_empty(),
        };

        if !has_positions {
            return ProcessResult::arrays(vec![Arc::new(array.clone())]);
        }

        // Frame ID tracking
        if self.expected_id > 0 {
            let uid = array.unique_id;
            if uid > self.expected_id {
                let diff = (uid - self.expected_id) as usize;
                self.missing_frames += diff;
                for _ in 0..diff {
                    self.advance();
                    let has = match self.mode {
                        PosMode::Discard => !self.positions.is_empty(),
                        PosMode::Keep => !self.all_positions.is_empty(),
                    };
                    if !has {
                        return ProcessResult::arrays(vec![Arc::new(array.clone())]);
                    }
                }
            } else if uid < self.expected_id {
                self.duplicate_frames += 1;
                return ProcessResult::empty();
            }
        }

        let position = match self.current_position() {
            Some(pos) => pos.clone(),
            None => return ProcessResult::arrays(vec![Arc::new(array.clone())]),
        };

        let mut out = array.clone();
        for (key, value) in &position {
            out.attributes.add(NDAttribute {
                name: key.clone(),
                description: String::new(),
                source: NDAttrSource::Driver,
                value: NDAttrValue::Float64(*value),
            });
        }

        self.advance();
        self.expected_id = array.unique_id + 1;

        let updates = vec![
            ParamUpdate::int32(0, self.missing_frames as i32),
            ParamUpdate::int32(1, self.duplicate_frames as i32),
        ];

        ProcessResult {
            output_arrays: vec![Arc::new(out)],
            param_updates: updates,
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPosPlugin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};

    fn make_array(id: i32) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr
    }

    #[test]
    fn test_discard_mode() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let mut pos1 = HashMap::new();
        pos1.insert("X".into(), 1.5);
        pos1.insert("Y".into(), 2.3);
        let mut pos2 = HashMap::new();
        pos2.insert("X".into(), 3.1);
        pos2.insert("Y".into(), 4.2);

        proc.load_positions(vec![pos1, pos2]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);

        let result = proc.process_array(&make_array(1), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 1.5).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 3.1).abs() < 1e-10);

        assert_eq!(proc.remaining_positions(), 0);
    }

    #[test]
    fn test_keep_mode() {
        let mut proc = PosPluginProcessor::new(PosMode::Keep);
        let mut pos1 = HashMap::new();
        pos1.insert("X".into(), 10.0);
        let mut pos2 = HashMap::new();
        pos2.insert("X".into(), 20.0);

        proc.load_positions(vec![pos1, pos2]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);

        let result = proc.process_array(&make_array(1), &pool);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 10.0).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 20.0).abs() < 1e-10);

        // Wraps around
        let result = proc.process_array(&make_array(3), &pool);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_missing_frames() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let mut pos1 = HashMap::new();
        pos1.insert("X".into(), 1.0);
        let mut pos2 = HashMap::new();
        pos2.insert("X".into(), 2.0);
        let mut pos3 = HashMap::new();
        pos3.insert("X".into(), 3.0);

        proc.load_positions(vec![pos1, pos2, pos3]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);

        proc.process_array(&make_array(1), &pool);

        // Frame 3 (skip frame 2)
        let result = proc.process_array(&make_array(3), &pool);
        assert_eq!(proc.missing_frames(), 1);
        let x = result.output_arrays[0].attributes.get("X").unwrap().value.as_f64().unwrap();
        assert!((x - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_duplicate_frames() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let mut pos1 = HashMap::new();
        pos1.insert("X".into(), 1.0);
        let mut pos2 = HashMap::new();
        pos2.insert("X".into(), 2.0);

        proc.load_positions(vec![pos1, pos2]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);

        proc.process_array(&make_array(1), &pool);

        let result = proc.process_array(&make_array(1), &pool);
        assert_eq!(proc.duplicate_frames(), 1);
        assert!(result.output_arrays.is_empty());
    }

    #[test]
    fn test_load_json() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let json = r#"{"positions": [{"X": 1.5, "Y": 2.3}, {"X": 3.1, "Y": 4.2}]}"#;
        let count = proc.load_positions_json(json).unwrap();
        assert_eq!(count, 2);
        assert_eq!(proc.remaining_positions(), 2);
    }

    #[test]
    fn test_not_running_passthrough() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&make_array(1), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.output_arrays[0].attributes.get("X").is_none());
    }
}
