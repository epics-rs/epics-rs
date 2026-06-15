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

    /// Load positions from an XML string (C++ NDPosPlugin `pos_layout` format).
    ///
    /// Expected XML format (matching `NDPosPluginFileReader`):
    /// ```xml
    /// <pos_layout>
    ///   <dimensions>
    ///     <dimension name="x"/>
    ///     <dimension name="y"/>
    ///   </dimensions>
    ///   <positions>
    ///     <position x="1" y="2"/>
    ///     <position x="3" y="4"/>
    ///   </positions>
    /// </pos_layout>
    /// ```
    ///
    /// Each `<dimension name="N"/>` declares an ordered dimension; each
    /// `<position .../>` carries one attribute per dimension, and the attribute
    /// value (parsed as f64) is stored under the dimension name. A position
    /// missing any declared dimension's attribute is rejected (matching C
    /// `addPosition` returning `asynError`).
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

/// Parse positions from the C++ NDPosPlugin `pos_layout` XML format.
///
/// Mirrors `NDPosPluginFileReader`: ordered dimension names are collected from
/// `<dimension name="N"/>` elements, then each `<position .../>` element is read
/// for one attribute per declared dimension, building a `map<dimension, value>`.
/// A position missing any declared dimension's attribute — or whose attribute
/// value does not parse as f64 — is rejected entirely (C `addPosition` returns
/// `asynError` and does not push that position).
///
/// This is a minimal hand-written parser for this trivial XML format, avoiding
/// the need for an external XML crate dependency.
fn parse_positions_xml(xml: &str) -> Result<Vec<HashMap<String, f64>>, String> {
    // Collect ordered dimension names from <dimension name="N"/> elements.
    let dimensions: Vec<String> = element_tag_contents(xml, "dimension")
        .into_iter()
        .filter_map(|content| parse_tag_attributes(content).remove("name"))
        .collect();

    let mut positions: Vec<HashMap<String, f64>> = Vec::new();
    for content in element_tag_contents(xml, "position") {
        let attrs = parse_tag_attributes(content);
        // C addPosition first requires the element to carry attributes at all.
        if attrs.is_empty() {
            continue;
        }
        let mut pos = HashMap::new();
        let mut ok = true;
        for dim in &dimensions {
            match attrs.get(dim).and_then(|v| v.parse::<f64>().ok()) {
                Some(value) => {
                    pos.insert(dim.clone(), value);
                }
                None => {
                    // Missing or unparseable dimension attribute → reject position.
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            positions.push(pos);
        }
    }

    Ok(positions)
}

/// True if `c` validly terminates the element name in `<name` of an opening tag:
/// whitespace (attributes follow), '>' (tag end), or '/' (self-closing). Used to
/// reject the longer-named sibling — e.g. `<positions`/`<dimensions` when
/// scanning for `<position`/`<dimension`.
fn is_tag_boundary(c: char) -> bool {
    c.is_ascii_whitespace() || c == '>' || c == '/'
}

/// Collect the attribute-region slice (the text between `<name` and `>`) of
/// every `<name ...>` opening tag, skipping any longer-named sibling
/// (`<names ...>`).
fn element_tag_contents<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let prefix = format!("<{}", name);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&prefix) {
        let open = from + rel;
        let after = open + prefix.len();
        match xml[after..].chars().next() {
            Some(c) if is_tag_boundary(c) => {}
            _ => {
                // <names ...> or end of string — not this element.
                from = after;
                continue;
            }
        }
        let Some(rel_end) = xml[after..].find('>') else {
            break;
        };
        let end = after + rel_end;
        out.push(&xml[after..end]);
        from = end + 1;
    }
    out
}

/// Parse `key="value"` / `key='value'` attribute pairs from a tag's
/// attribute region.
fn parse_tag_attributes(content: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while let Some(eq_rel) = content[i..].find('=') {
        let eq = i + eq_rel;
        // Key: the identifier immediately preceding '=' (skipping whitespace).
        let mut k_end = eq;
        while k_end > i && bytes[k_end - 1].is_ascii_whitespace() {
            k_end -= 1;
        }
        let mut k_start = k_end;
        while k_start > i
            && !bytes[k_start - 1].is_ascii_whitespace()
            && bytes[k_start - 1] != b'/'
            && bytes[k_start - 1] != b'='
        {
            k_start -= 1;
        }
        let key = &content[k_start..k_end];
        // Value: quoted string after '='.
        let mut j = eq + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let quote = bytes[j];
        if quote != b'"' && quote != b'\'' {
            i = eq + 1;
            continue;
        }
        j += 1;
        let val_start = j;
        while j < bytes.len() && bytes[j] != quote {
            j += 1;
        }
        if j >= bytes.len() {
            break; // unterminated quote
        }
        if !key.is_empty() {
            attrs.insert(key.to_string(), content[val_start..j].to_string());
        }
        i = j + 1;
    }
    attrs
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
            // C NDPosPlugin.cpp:161 constructs each attribute with the fixed
            // description "Position of NDArray".
            out.attributes.add(NDAttribute::new_static(
                key.clone(),
                "Position of NDArray",
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
    fn test_attribute_description() {
        // C NDPosPlugin.cpp:161 sets the attribute description "Position of NDArray".
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let mut pos = HashMap::new();
        pos.insert("X".into(), 1.5);
        proc.load_positions(vec![pos]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&make_array(1), &pool);
        let attr = result.output_arrays[0].attributes.get("X").unwrap();
        assert_eq!(attr.description, "Position of NDArray");
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
        let xml = r#"<pos_layout>
  <dimensions>
    <dimension name="x"/>
  </dimensions>
  <positions>
    <position x="1.5"/>
    <position x="2.3"/>
    <position x="3.7"/>
  </positions>
</pos_layout>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 3);
        assert_eq!(proc.remaining_positions(), 3);
    }

    #[test]
    fn test_load_xml_dimension_keyed() {
        // C NDPosPluginFileReader keys each position attribute by dimension name
        // and keeps positions in document order (no index sort).
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<pos_layout>
  <dimensions>
    <dimension name="x"/>
    <dimension name="y"/>
  </dimensions>
  <positions>
    <position x="10" y="100"/>
    <position x="20" y="200"/>
  </positions>
</pos_layout>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 2);

        proc.start();
        let pool = NDArrayPool::new(1_000_000);

        let result = proc.process_array(&make_array(1), &pool);
        let attrs = &result.output_arrays[0].attributes;
        assert!((attrs.get("x").unwrap().value.as_f64().unwrap() - 10.0).abs() < 1e-10);
        assert!((attrs.get("y").unwrap().value.as_f64().unwrap() - 100.0).abs() < 1e-10);

        let result = proc.process_array(&make_array(2), &pool);
        let attrs = &result.output_arrays[0].attributes;
        assert!((attrs.get("x").unwrap().value.as_f64().unwrap() - 20.0).abs() < 1e-10);
        assert!((attrs.get("y").unwrap().value.as_f64().unwrap() - 200.0).abs() < 1e-10);
    }

    #[test]
    fn test_load_xml_rejects_incomplete_position() {
        // A position missing a declared dimension's attribute is rejected whole
        // (C addPosition returns asynError), matching the per-position drop.
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<pos_layout>
  <dimensions>
    <dimension name="x"/>
    <dimension name="y"/>
  </dimensions>
  <positions>
    <position x="1" y="2"/>
    <position x="3"/>
    <position x="5" y="6"/>
  </positions>
</pos_layout>"#;
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
        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions><position x="99.9"/></positions></pos_layout>"#;
        let count = proc.load_positions_auto(xml).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_load_xml_empty() {
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions></positions></pos_layout>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 0);
    }
}
