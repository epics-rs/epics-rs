use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};
use parking_lot::Mutex;
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

/// Asyn param indices for the 17 NDPosPlugin params (resolved in
/// `register_params`). Names match the C `str_NDPos_*` drvInfo strings
/// (`NDPosPlugin.h:39-55`) so the `NDPosPlugin.template` records bind.
#[derive(Default)]
struct PosParamIndices {
    filename: Option<usize>,
    file_valid: Option<usize>,
    clear: Option<usize>,
    running: Option<usize>,
    restart: Option<usize>,
    delete: Option<usize>,
    mode: Option<usize>,
    append: Option<usize>,
    current_qty: Option<usize>,
    current_index: Option<usize>,
    current_pos: Option<usize>,
    missing_frames: Option<usize>,
    duplicate_frames: Option<usize>,
    expected_id: Option<usize>,
    id_name: Option<usize>,
    id_difference: Option<usize>,
    id_start: Option<usize>,
}

/// NDPosPlugin processor: attaches position metadata to arrays from a position list.
pub struct PosPluginProcessor {
    state: Mutex<PosPluginState>,
}

/// The position cursor and the frame-ID tracking counters. One frame consumes
/// a position, advances the cursor and posts the counters together.
struct PosPluginState {
    positions: VecDeque<HashMap<String, f64>>,
    all_positions: Vec<HashMap<String, f64>>,
    mode: PosMode,
    index: usize,
    running: bool,
    expected_id: i32,
    /// C `NDPos_IDStart` (default 1): the value `ExpectedID` is reset to on
    /// every Running write (NDPosPlugin.cpp:420,232-234).
    id_start: i32,
    /// C `NDPos_IDDifference` (default 1): the step `ExpectedID` advances by
    /// per frame and per dropped frame (NDPosPlugin.cpp:103,115,193).
    id_difference: i32,
    missing_frames: usize,
    duplicate_frames: usize,
    params: PosParamIndices,
}

impl PosPluginProcessor {
    pub fn new(mode: PosMode) -> Self {
        Self {
            state: Mutex::new(PosPluginState::new(mode)),
        }
    }

    /// Load positions from a JSON string.
    pub fn load_positions_json(&self, json_str: &str) -> Result<usize, serde_json::Error> {
        self.state.lock().load_positions_json(json_str)
    }

    /// Load positions from an XML string (C++ NDPosPlugin `pos_layout` format).
    pub fn load_positions_xml(&self, xml_str: &str) -> Result<usize, String> {
        self.state.lock().load_positions_xml(xml_str)
    }

    /// Load positions from a string, auto-detecting format.
    pub fn load_positions_auto(&self, content: &str) -> Result<usize, String> {
        self.state.lock().load_positions_auto(content)
    }

    /// Load positions directly.
    pub fn load_positions(&self, positions: Vec<HashMap<String, f64>>) {
        self.state.lock().load_positions(positions);
    }

    /// Start processing.
    pub fn start(&self) {
        self.state.lock().start();
    }

    /// Stop processing.
    pub fn stop(&self) {
        self.state.lock().stop();
    }

    /// Clear all positions.
    pub fn clear(&self) {
        self.state.lock().clear();
    }

    pub fn missing_frames(&self) -> usize {
        self.state.lock().missing_frames
    }

    pub fn duplicate_frames(&self) -> usize {
        self.state.lock().duplicate_frames
    }

    pub fn remaining_positions(&self) -> usize {
        self.state.lock().remaining_positions()
    }
}

impl PosPluginState {
    fn new(mode: PosMode) -> Self {
        Self {
            positions: VecDeque::new(),
            all_positions: Vec::new(),
            mode,
            index: 0,
            running: false,
            expected_id: 0,
            id_start: 1,
            id_difference: 1,
            missing_frames: 0,
            duplicate_frames: 0,
            params: PosParamIndices::default(),
        }
    }

    /// Load positions from a JSON string.
    fn load_positions_json(&mut self, json_str: &str) -> Result<usize, serde_json::Error> {
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
    fn load_positions_xml(&mut self, xml_str: &str) -> Result<usize, String> {
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
    fn load_positions_auto(&mut self, content: &str) -> Result<usize, String> {
        if content.trim_start().starts_with('<') {
            self.load_positions_xml(content)
        } else {
            self.load_positions_json(content)
                .map_err(|e| format!("JSON parse error: {}", e))
        }
    }

    /// Load positions directly.
    fn load_positions(&mut self, positions: Vec<HashMap<String, f64>>) {
        self.all_positions = positions.clone();
        self.positions = positions.into();
        self.index = 0;
    }

    /// Start processing.
    ///
    /// C `writeInt32(NDPos_Running)` resets `ExpectedID` to `IDStart` and does
    /// nothing else (NDPosPlugin.cpp:230-235) — in particular it does *not*
    /// clear MissingFrames/DuplicateFrames, which persist across runs until a
    /// client writes them (they are zeroed only in the constructor).
    fn start(&mut self) {
        self.running = true;
        self.expected_id = self.id_start;
    }

    /// Stop processing.
    fn stop(&mut self) {
        self.running = false;
    }

    /// Clear all positions.
    fn clear(&mut self) {
        self.positions.clear();
        self.all_positions.clear();
        self.index = 0;
    }

    fn remaining_positions(&self) -> usize {
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

    /// Whether a position remains to be consumed at the current cursor — C's
    /// `size > 0` (Discard) / `index < size` (Keep) guard
    /// (NDPosPlugin.cpp:99,113).
    fn has_position(&self) -> bool {
        match self.mode {
            PosMode::Discard => !self.positions.is_empty(),
            PosMode::Keep => self.index < self.all_positions.len(),
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

    /// The value C reports in `NDPos_CurrentIndex`: 0 in Discard mode (the
    /// cursor never moves, the front is always consumed) and the running index
    /// in Keep mode (NDPosPlugin.cpp:190; Discard never sets CurrentIndex).
    fn current_index_param(&self) -> i32 {
        match self.mode {
            PosMode::Discard => 0,
            PosMode::Keep => self.index as i32,
        }
    }

    /// C exhaustion path: positions ran out, so set `NDPos_Running = IDLE` and
    /// emit no downstream callback for this frame (NDPosPlugin.cpp:56-59,
    /// 107-122,197-204).
    fn exhausted_result(&mut self) -> ProcessResult {
        self.running = false;
        let mut updates = Vec::new();
        push_int(&mut updates, self.params.running, 0);
        ProcessResult {
            output_arrays: vec![],
            param_updates: updates,
            scatter: false,
        }
    }
}

/// Push an `Int32` param update only when the param index is resolved.
fn push_int(updates: &mut Vec<ParamUpdate>, idx: Option<usize>, value: i32) {
    if let Some(i) = idx {
        updates.push(ParamUpdate::int32(i, value));
    }
}

/// Push an `Octet` param update only when the param index is resolved.
fn push_str(updates: &mut Vec<ParamUpdate>, idx: Option<usize>, value: String) {
    if let Some(i) = idx {
        updates.push(ParamUpdate::octet(i, value));
    }
}

/// Format `v` the way C++ `std::ostream << double` does by default
/// (`defaultfloat`, stream precision 6) — equivalent to C `printf("%g", v)`.
/// C builds the NDPos_CurrentPos string by streaming each position double
/// (NDPosPlugin.cpp:159), so the observable octet value must use this format
/// rather than Rust's shortest-round-trip `Display`.
fn format_cpp_g6(v: f64) -> String {
    const PREC: i32 = 6;
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    // Round to PREC significant figures via scientific form, then read the
    // decimal exponent — this avoids log10 floor off-by-one at powers of ten.
    let sci = format!("{:.*e}", (PREC - 1) as usize, v);
    let epos = sci.find('e').unwrap();
    let exp: i32 = sci[epos + 1..].parse().unwrap();
    if exp >= -4 && exp < PREC {
        // %f branch: precision = PREC-1-exp digits after the point.
        let prec = (PREC - 1 - exp).max(0) as usize;
        strip_g_trailing(&format!("{:.*}", prec, v))
    } else {
        // %e branch: strip mantissa trailing zeros, render a signed 2+digit
        // exponent (C printf style: "1.23457e+06", "1e-05").
        let mantissa = strip_g_trailing(&sci[..epos]);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", mantissa, sign, exp.abs())
    }
}

/// Strip `%g` trailing zeros: remove trailing '0' digits after a '.', then a
/// dangling '.'. No-op for strings without a decimal point.
fn strip_g_trailing(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
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
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let mut state = self.state.lock();
        if !state.running {
            // C only reaches endProcessCallbacks inside `if (running ==
            // NDPOS_RUNNING)` (NDPosPlugin.cpp:54,202); an idle plugin emits no
            // downstream callback, it does not pass the frame through.
            return ProcessResult::empty();
        }

        // C checks `index >= size` up front and, when the list is already
        // exhausted, sets Running=IDLE and emits nothing (NDPosPlugin.cpp:56-59,
        // 197-200).
        if !state.has_position() {
            return state.exhausted_result();
        }

        // Frame ID tracking. C compares against ExpectedID = IDStart from the
        // very first running frame (NDPosPlugin.cpp:90-135); there is no
        // "skip the first frame" gate, so a first frame whose uniqueId differs
        // from IDStart is classified as missing/duplicate immediately.
        let uid = array.unique_id;
        if uid > state.expected_id {
            // Missing frame(s): step ExpectedID by IDDifference, dropping one
            // position per step, until we catch up or run out
            // (NDPosPlugin.cpp:91-127). ExpectedID is never re-anchored to uid.
            while state.expected_id < uid && state.has_position() {
                state.advance();
                state.expected_id += state.id_difference;
                state.missing_frames += 1;
            }
            if !state.has_position() {
                // Positions exhausted mid-gap: C stops and drops the frame
                // (NDPosPlugin.cpp:107-110,119-122).
                return state.exhausted_result();
            }
        } else if uid < state.expected_id {
            state.duplicate_frames += 1;
            // C sets skip=1 (no downstream emit) but still posts
            // DuplicateFrames (NDPosPlugin.cpp:132-135).
            let mut updates = Vec::new();
            push_int(
                &mut updates,
                state.params.duplicate_frames,
                state.duplicate_frames as i32,
            );
            return ProcessResult {
                output_arrays: vec![],
                param_updates: updates,
                scatter: false,
            };
        }

        // Guaranteed `Some` here: has_position() was rechecked above.
        let position = state.current_position().unwrap().clone();

        let mut out = array.clone();
        // C iterates the position std::map (sorted ascending by key), building
        // the CurrentPos string "[k=v,...]" and attaching each attribute in the
        // same loop (NDPosPlugin.cpp:149-166). The attribute description is the
        // fixed "Position of NDArray" (line 161).
        let mut keys: Vec<&String> = position.keys().collect();
        keys.sort();
        let mut current_pos = String::from("[");
        for (n, key) in keys.iter().enumerate() {
            let value = position[*key];
            if n > 0 {
                current_pos.push(',');
            }
            current_pos.push_str(key);
            current_pos.push('=');
            current_pos.push_str(&format_cpp_g6(value));
            out.attributes.add(NDAttribute::new_static(
                (*key).clone(),
                "Position of NDArray",
                NDAttrSource::Driver,
                NDAttrValue::Float64(value),
            ));
        }
        current_pos.push(']');

        state.advance();
        // C steps ExpectedID by IDDifference (NDPosPlugin.cpp:193), it does not
        // re-anchor to the received uniqueId.
        state.expected_id += state.id_difference;

        // C posts MissingFrames/DuplicateFrames and, after advancing, the new
        // CurrentQty (Discard) / CurrentIndex (Keep) (NDPosPlugin.cpp:126,134,
        // 187,190).
        let mut updates = Vec::new();
        push_int(
            &mut updates,
            state.params.missing_frames,
            state.missing_frames as i32,
        );
        push_int(
            &mut updates,
            state.params.duplicate_frames,
            state.duplicate_frames as i32,
        );
        push_int(
            &mut updates,
            state.params.current_qty,
            state.remaining_positions() as i32,
        );
        push_int(
            &mut updates,
            state.params.current_index,
            state.current_index_param(),
        );
        // C setStringParam(NDPos_CurrentPos, ...) (NDPosPlugin.cpp:166).
        push_str(&mut updates, state.params.current_pos, current_pos);

        ProcessResult {
            output_arrays: vec![Arc::new(out)],
            param_updates: updates,
            scatter: false,
        }
    }

    fn plugin_type(&self) -> &str {
        // C sets PluginType to "NDPositionPlugin" (NDPosPlugin.cpp:402), not the
        // class name.
        "NDPositionPlugin"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        let state = self.state.get_mut();
        // 17 params in C createParam order (NDPosPlugin.cpp:383-399).
        base.create_param("NDPos_Filename", ParamType::Octet)?;
        base.create_param("NDPos_FileValid", ParamType::Int32)?;
        base.create_param("NDPos_Clear", ParamType::Int32)?;
        base.create_param("NDPos_Running", ParamType::Int32)?;
        base.create_param("NDPos_Restart", ParamType::Int32)?;
        base.create_param("NDPos_Delete", ParamType::Int32)?;
        base.create_param("NDPos_Mode", ParamType::Int32)?;
        base.create_param("NDPos_Append", ParamType::Int32)?;
        base.create_param("NDPos_CurrentQty", ParamType::Int32)?;
        base.create_param("NDPos_CurrentIndex", ParamType::Int32)?;
        base.create_param("NDPos_CurrentPos", ParamType::Octet)?;
        base.create_param("NDPos_MissingFrames", ParamType::Int32)?;
        base.create_param("NDPos_DuplicateFrames", ParamType::Int32)?;
        base.create_param("NDPos_ExpectedID", ParamType::Int32)?;
        base.create_param("NDPos_IDName", ParamType::Octet)?;
        base.create_param("NDPos_IDDifference", ParamType::Int32)?;
        base.create_param("NDPos_IDStart", ParamType::Int32)?;

        state.params.filename = base.find_param("NDPos_Filename");
        state.params.file_valid = base.find_param("NDPos_FileValid");
        state.params.clear = base.find_param("NDPos_Clear");
        state.params.running = base.find_param("NDPos_Running");
        state.params.restart = base.find_param("NDPos_Restart");
        state.params.delete = base.find_param("NDPos_Delete");
        state.params.mode = base.find_param("NDPos_Mode");
        state.params.append = base.find_param("NDPos_Append");
        state.params.current_qty = base.find_param("NDPos_CurrentQty");
        state.params.current_index = base.find_param("NDPos_CurrentIndex");
        state.params.current_pos = base.find_param("NDPos_CurrentPos");
        state.params.missing_frames = base.find_param("NDPos_MissingFrames");
        state.params.duplicate_frames = base.find_param("NDPos_DuplicateFrames");
        state.params.expected_id = base.find_param("NDPos_ExpectedID");
        state.params.id_name = base.find_param("NDPos_IDName");
        state.params.id_difference = base.find_param("NDPos_IDDifference");
        state.params.id_start = base.find_param("NDPos_IDStart");

        // C constructor defaults (NDPosPlugin.cpp:402-426).
        if let Some(i) = state.params.mode {
            base.set_int32_param(i, 0, state.mode as i32)?;
        }
        if let Some(i) = state.params.file_valid {
            base.set_int32_param(i, 0, 0)?;
        }
        if let Some(i) = state.params.current_index {
            base.set_int32_param(i, 0, 0)?;
        }
        if let Some(i) = state.params.current_qty {
            base.set_int32_param(i, 0, state.remaining_positions() as i32)?;
        }
        if let Some(i) = state.params.current_pos {
            base.set_string_param(i, 0, String::new())?;
        }
        if let Some(i) = state.params.running {
            base.set_int32_param(i, 0, 0)?;
        }
        if let Some(i) = state.params.id_name {
            base.set_string_param(i, 0, String::new())?;
        }
        if let Some(i) = state.params.id_difference {
            base.set_int32_param(i, 0, 1)?;
        }
        if let Some(i) = state.params.id_start {
            base.set_int32_param(i, 0, 1)?;
        }
        if let Some(i) = state.params.expected_id {
            base.set_int32_param(i, 0, 1)?;
        }
        if let Some(i) = state.params.missing_frames {
            base.set_int32_param(i, 0, 0)?;
        }
        if let Some(i) = state.params.duplicate_frames {
            base.set_int32_param(i, 0, 0)?;
        }
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue};

        let mut state = self.state.lock();
        let mut updates = Vec::new();
        if Some(reason) == state.params.running {
            // C writeInt32(NDPos_Running): start/stop and reset ExpectedID to
            // IDStart (NDPosPlugin.cpp:230-234).
            if params.value.as_i32() == 0 {
                state.stop();
            } else {
                state.start();
                push_int(&mut updates, state.params.expected_id, state.id_start);
            }
        } else if Some(reason) == state.params.id_start {
            // C stores IDStart; it is read on the next Running write.
            state.id_start = params.value.as_i32();
        } else if Some(reason) == state.params.id_difference {
            // C stores IDDifference; it is read each processCallbacks.
            state.id_difference = params.value.as_i32();
        } else if Some(reason) == state.params.mode {
            // C writeInt32(NDPos_Mode): reset index to 0 (NDPosPlugin.cpp:235-237).
            state.mode = match params.value.as_i32() {
                1 => PosMode::Keep,
                _ => PosMode::Discard,
            };
            state.index = 0;
            push_int(&mut updates, state.params.current_index, 0);
        } else if Some(reason) == state.params.restart {
            // C writeInt32(NDPos_Restart): reset index, clear CurrentPos
            // (NDPosPlugin.cpp:238-242).
            state.index = 0;
            push_int(&mut updates, state.params.current_index, 0);
            push_str(&mut updates, state.params.current_pos, String::new());
        } else if Some(reason) == state.params.delete {
            // C writeInt32(NDPos_Delete): reset index, clear CurrentPos, clear
            // positions, CurrentQty=0 (NDPosPlugin.cpp:243-250).
            state.clear();
            push_int(&mut updates, state.params.current_index, 0);
            push_int(&mut updates, state.params.current_qty, 0);
            push_str(&mut updates, state.params.current_pos, String::new());
        } else if Some(reason) == state.params.filename {
            // C writeOctet(NDPos_Filename): validate + load XML, set FileValid,
            // append positions, set CurrentQty (NDPosPlugin.cpp:295-315).
            if let ParamChangeValue::Octet(ref xml) = params.value {
                match state.load_positions_auto(xml) {
                    Ok(_) => {
                        push_int(&mut updates, state.params.file_valid, 1);
                        push_int(
                            &mut updates,
                            state.params.current_qty,
                            state.remaining_positions() as i32,
                        );
                    }
                    Err(_) => {
                        push_int(&mut updates, state.params.file_valid, 0);
                    }
                }
            }
        }
        ParamChangeResult::updates(updates)
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Keep);
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

        // Stops at end of list (no wrapping): the exhausted frame is dropped and
        // the plugin goes idle (ADP-38).
        let result = proc.process_array(&make_array(3), &pool);
        assert!(result.output_arrays.is_empty());
        assert!(!proc.state.lock().running);
    }

    #[test]
    fn test_exhaustion_stops_and_drops() {
        // ADP-38: when positions run out, C sets Running=IDLE and emits no
        // downstream callback (no bare frame forwarded).
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().params.running = Some(5);
        let mut p1 = HashMap::new();
        p1.insert("x".into(), 1.0);
        proc.load_positions(vec![p1]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        // Frame 1 consumes the only position.
        let r1 = proc.process_array(&make_array(1), &pool);
        assert_eq!(r1.output_arrays.len(), 1);
        // Frame 2 finds no positions: dropped, Running posted IDLE, plugin idle.
        let r2 = proc.process_array(&make_array(2), &pool);
        assert!(r2.output_arrays.is_empty());
        assert!(!proc.state.lock().running);
        use ad_core_rs::plugin::runtime::ParamUpdate;
        assert!(r2.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 5,
                value: 0,
                ..
            }
        )));
    }

    #[test]
    fn test_missing_frames() {
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let json = r#"{"positions": [{"X": 1.5, "Y": 2.3}, {"X": 3.1, "Y": 4.2}]}"#;
        let count = proc.load_positions_json(json).unwrap();
        assert_eq!(count, 2);
        assert_eq!(proc.remaining_positions(), 2);
    }

    #[test]
    fn test_idle_drops_frame() {
        // ADP-35: when idle (not running) C emits no downstream callback; the
        // frame is dropped, not passed through.
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&make_array(1), &pool);
        assert!(result.output_arrays.is_empty());
    }

    #[test]
    fn test_load_xml() {
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
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
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let json = r#"{"positions": [{"X": 1.5}]}"#;
        let count = proc.load_positions_auto(json).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_load_auto_xml() {
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions><position x="99.9"/></positions></pos_layout>"#;
        let count = proc.load_positions_auto(xml).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_load_xml_empty() {
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions></positions></pos_layout>"#;
        let count = proc.load_positions_xml(xml).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_param_posts_to_registered_indices() {
        // ADP-33: posts land on the registered MissingFrames/DuplicateFrames/
        // CurrentQty indices, never the old hardcoded 0/1.
        use ad_core_rs::plugin::runtime::ParamUpdate;
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().params.missing_frames = Some(20);
        proc.state.lock().params.duplicate_frames = Some(21);
        proc.state.lock().params.current_qty = Some(22);

        let mut p1 = HashMap::new();
        p1.insert("x".into(), 1.0);
        let mut p2 = HashMap::new();
        p2.insert("x".into(), 2.0);
        proc.load_positions(vec![p1, p2]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&make_array(1), &pool);
        // CurrentQty drops to 1 remaining after consuming the first position.
        assert!(
            result
                .param_updates
                .iter()
                .any(|u| matches!(u, ParamUpdate::Int32 { reason: 20, .. }))
        );
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 22,
                value: 1,
                ..
            }
        )));
        assert!(
            !result
                .param_updates
                .iter()
                .any(|u| matches!(u, ParamUpdate::Int32 { reason: 0, .. }))
        );
    }

    #[test]
    fn test_filename_param_loads_positions() {
        // ADP-33: writing NDPos_Filename loads the XML, posts FileValid=1 + CurrentQty.
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().params.filename = Some(0);
        proc.state.lock().params.file_valid = Some(1);
        proc.state.lock().params.current_qty = Some(8);

        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions><position x="1"/><position x="2"/></positions></pos_layout>"#;
        let snapshot = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 0,
            addr: 0,
            value: ParamChangeValue::Octet(xml.to_string()),
        };
        let result = proc.on_param_change(0, &snapshot);
        assert_eq!(proc.remaining_positions(), 2);
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 1,
                value: 1,
                ..
            }
        )));
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 8,
                value: 2,
                ..
            }
        )));
    }

    #[test]
    fn test_running_param_starts_and_stops() {
        // ADP-33: writing NDPos_Running routes to start()/stop().
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().params.running = Some(3);

        let start = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 3,
            addr: 0,
            value: ParamChangeValue::Int32(1),
        };
        proc.on_param_change(3, &start);
        assert!(proc.state.lock().running);

        let stop = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 3,
            addr: 0,
            value: ParamChangeValue::Int32(0),
        };
        proc.on_param_change(3, &stop);
        assert!(!proc.state.lock().running);
    }

    #[test]
    fn test_current_pos_string_posted() {
        // ADP-32: process_array posts NDPos_CurrentPos as "[k=v,...]" in sorted
        // key order (C std::map), C++ %g(6) value formatting.
        use ad_core_rs::plugin::runtime::ParamUpdate;
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().params.current_pos = Some(30);

        let mut p = HashMap::new();
        p.insert("y".into(), 2.0);
        p.insert("x".into(), 1.5);
        proc.load_positions(vec![p]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&make_array(1), &pool);
        let s = result.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Octet {
                reason: 30, value, ..
            } => Some(value.clone()),
            _ => None,
        });
        assert_eq!(s.as_deref(), Some("[x=1.5,y=2]"));
    }

    #[test]
    fn test_first_frame_id_checked() {
        // ADP-37: ExpectedID starts at IDStart (1); a first running frame whose
        // uniqueId != IDStart is classified immediately (here frames 1,2 are
        // missing), not silently accepted.
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let mut p1 = HashMap::new();
        p1.insert("x".into(), 1.0);
        let mut p2 = HashMap::new();
        p2.insert("x".into(), 2.0);
        let mut p3 = HashMap::new();
        p3.insert("x".into(), 3.0);
        proc.load_positions(vec![p1, p2, p3]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        // First frame arrives as uniqueId 3 → frames 1 and 2 counted missing.
        let result = proc.process_array(&make_array(3), &pool);
        assert_eq!(proc.missing_frames(), 2);
        let x = result.output_arrays[0]
            .attributes
            .get("x")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_id_difference_stepping() {
        // ADP-36: ExpectedID steps by IDDifference and is never re-anchored to
        // uniqueId. With step 2, frames 1/3/5 are all on-sequence (no missing).
        let proc = PosPluginProcessor::new(PosMode::Discard);
        proc.state.lock().id_difference = 2;
        let mut p1 = HashMap::new();
        p1.insert("x".into(), 1.0);
        let mut p2 = HashMap::new();
        p2.insert("x".into(), 2.0);
        let mut p3 = HashMap::new();
        p3.insert("x".into(), 3.0);
        proc.load_positions(vec![p1, p2, p3]);
        proc.start();

        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&make_array(1), &pool);
        proc.process_array(&make_array(3), &pool);
        let r = proc.process_array(&make_array(5), &pool);
        assert_eq!(proc.missing_frames(), 0);
        let x = r.output_arrays[0]
            .attributes
            .get("x")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_format_cpp_g6() {
        // Matches C printf("%g") / C++ default ostream<<double (precision 6).
        assert_eq!(format_cpp_g6(1.0), "1");
        assert_eq!(format_cpp_g6(1.5), "1.5");
        assert_eq!(format_cpp_g6(42.5), "42.5");
        assert_eq!(format_cpp_g6(100.0), "100");
        assert_eq!(format_cpp_g6(0.1), "0.1");
        assert_eq!(format_cpp_g6(0.0001), "0.0001");
        assert_eq!(format_cpp_g6(0.00001), "1e-05");
        assert_eq!(format_cpp_g6(1_000_000.0), "1e+06");
        assert_eq!(format_cpp_g6(1_234_567.0), "1.23457e+06");
        assert_eq!(format_cpp_g6(123456.0), "123456");
        assert_eq!(format_cpp_g6(-1.5), "-1.5");
        assert_eq!(format_cpp_g6(0.0), "0");
    }
}
