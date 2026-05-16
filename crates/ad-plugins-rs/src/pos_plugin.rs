use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};
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

    /// Load positions from an XML string (C++ NDPosPlugin format).
    ///
    /// Expected XML format:
    /// ```xml
    /// <positions>
    ///   <position index="0">value1</position>
    ///   <position index="1">value2</position>
    /// </positions>
    /// ```
    ///
    /// Each `<position>` element becomes a single-entry HashMap with key "position"
    /// mapped to the parsed f64 value. If the value cannot be parsed as f64,
    /// the position is skipped.
    pub fn load_positions_xml(&mut self, xml_str: &str) -> Result<usize, String> {
        let positions = parse_positions_xml(xml_str)?;
        let count = positions.len();
        self.all_positions = positions.clone();
        self.positions = positions.into();
        self.index = 0;
        Ok(count)
    }

    /// Load positions from a string, auto-detecting format.
    ///
    /// If the content starts with '<' (after trimming whitespace), it is treated as XML.
    /// Otherwise, it is treated as JSON.
    pub fn load_positions_auto(&mut self, content: &str) -> Result<usize, String> {
        if content.trim_start().starts_with('<') {
            self.load_positions_xml(content)
        } else {
            self.load_positions_json(content)
                .map_err(|e| format!("JSON parse error: {}", e))
        }
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
                if self.index < self.all_positions.len() {
                    Some(&self.all_positions[self.index])
                } else {
                    None
                }
            }
        }
    }

    fn advance(&mut self) {
        match self.mode {
            PosMode::Discard => {
                self.positions.pop_front();
            }
            PosMode::Keep => {
                self.index += 1;
            }
        }
    }
}

/// Parse positions from the C++ NDPosPlugin XML format.
///
/// Handles the simple format:
/// ```xml
/// <positions>
///   <position index="0">123.45</position>
///   <position index="1">678.90</position>
/// </positions>
/// ```
///
/// This is a minimal hand-written parser for this trivial XML format,
/// avoiding the need for an external XML crate dependency.
/// Check if a character can follow `<position` in a valid opening tag.
/// Valid: whitespace (attributes), '>' (end of tag), '/' (self-closing).
/// Invalid: 's' (which would mean `<positions`).
fn is_position_tag_boundary(c: char) -> bool {
    c.is_ascii_whitespace() || c == '>' || c == '/'
}

fn parse_positions_xml(xml: &str) -> Result<Vec<HashMap<String, f64>>, String> {
    let mut positions: Vec<(usize, f64)> = Vec::new();
    let tag_prefix = "<position";

    // Find all <position ...>value</position> elements
    let mut search_from = 0;
    while let Some(rel_start) = xml[search_from..].find(tag_prefix) {
        let open_start = search_from + rel_start;
        let after_prefix = open_start + tag_prefix.len();

        // Check that this is actually <position ...> and not <positions> or </positions>
        if after_prefix >= xml.len() {
            break;
        }
        let next_char = xml[after_prefix..].chars().next().unwrap_or(' ');
        if !is_position_tag_boundary(next_char) {
            // This is <positions> or similar, skip past it
            search_from = after_prefix;
            continue;
        }

        let tag_end = xml[open_start..]
            .find('>')
            .ok_or_else(|| "Malformed XML: unclosed <position tag".to_string())?;
        let tag_end = open_start + tag_end;

        // Parse index attribute from the opening tag
        let tag_content = &xml[open_start..tag_end];
        let index = if let Some(idx_start) = tag_content.find("index=") {
            let after_eq = &tag_content[idx_start + 6..];
            // Handle both index="0" and index='0'
            let quote_char = after_eq.chars().next().unwrap_or('"');
            if quote_char == '"' || quote_char == '\'' {
                let inner = &after_eq[1..];
                let end = inner.find(quote_char).ok_or_else(|| {
                    "Malformed XML: unclosed quote in index attribute".to_string()
                })?;
                inner[..end]
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid index value: {}", e))?
            } else {
                // No quotes, read digits
                let end = after_eq
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(after_eq.len());
                after_eq[..end]
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid index value: {}", e))?
            }
        } else {
            // No index attribute, use sequential ordering
            positions.len()
        };

        // Extract value between > and </position>
        let value_start = tag_end + 1;
        let close_tag = xml[value_start..]
            .find("</position>")
            .ok_or_else(|| "Malformed XML: missing </position> closing tag".to_string())?;
        let value_str = xml[value_start..value_start + close_tag].trim();

        if let Ok(value) = value_str.parse::<f64>() {
            positions.push((index, value));
        }
        // Skip non-numeric values silently

        search_from = value_start + close_tag + "</position>".len();
    }

    // Sort by index and build the result
    positions.sort_by_key(|(idx, _)| *idx);

    let result: Vec<HashMap<String, f64>> = positions
        .into_iter()
        .map(|(_, value)| {
            let mut map = HashMap::new();
            map.insert("position".into(), value);
            map
        })
        .collect();

    Ok(result)
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
            out.attributes.add(NDAttribute::new_static(
                key.clone(),
                String::new(),
                NDAttrSource::Driver,
                NDAttrValue::Float64(*value),
            ));
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
            scatter_index: None,
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPosPlugin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

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
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 1.5).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
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
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 10.0).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 20.0).abs() < 1e-10);

        // Stops at end of list (no wrapping)
        let result = proc.process_array(&make_array(3), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.output_arrays[0].attributes.get("X").is_none());
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
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
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

    #[test]
    fn test_load_xml() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<positions>
  <position index="0">1.5</position>
  <position index="1">2.3</position>
  <position index="2">3.7</position>
</positions>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 3);
        assert_eq!(proc.remaining_positions(), 3);
    }

    #[test]
    fn test_load_xml_out_of_order() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<positions>
  <position index="2">30.0</position>
  <position index="0">10.0</position>
  <position index="1">20.0</position>
</positions>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 3);

        proc.start();
        let pool = NDArrayPool::new(1_000_000);

        // Should be sorted by index: 10.0, 20.0, 30.0
        let result = proc.process_array(&make_array(1), &pool);
        let pos = result.output_arrays[0]
            .attributes
            .get("position")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((pos - 10.0).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let pos = result.output_arrays[0]
            .attributes
            .get("position")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((pos - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_xml_no_index() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<positions>
  <position>5.5</position>
  <position>6.6</position>
</positions>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_load_auto_json() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let json = r#"{"positions": [{"X": 1.5}]}"#;
        let count = proc.load_positions_auto(json).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_load_auto_xml() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<positions><position index="0">99.9</position></positions>"#;
        let count = proc.load_positions_auto(xml).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_load_xml_empty() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<positions></positions>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 0);
    }
}
