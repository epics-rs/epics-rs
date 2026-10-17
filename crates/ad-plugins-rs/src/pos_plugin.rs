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
#[derive(Default, Clone, Copy)]
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
    attributes: Option<ad_core_rs::plugin::runtime::PluginAttributes>,
}

/// The position cursor and the frame-ID tracking counters. One frame consumes
/// a position, advances the cursor and posts the counters together.
struct PosPluginState {
    /// C `positionArray`: one list for both modes, read at `index`.
    positions: VecDeque<HashMap<String, f64>>,
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
    missing_frames: i32,
    duplicate_frames: i32,
    params: PosParamIndices,
}

impl PosPluginProcessor {
    pub fn new(mode: PosMode) -> Self {
        Self {
            state: Mutex::new(PosPluginState::new(mode)),
            attributes: None,
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

    pub fn missing_frames(&self) -> i32 {
        self.state.lock().missing_frames
    }

    pub fn duplicate_frames(&self) -> i32 {
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
        Ok(self.append(list.positions))
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
        Ok(self.append(parse_positions_xml(xml_str)?))
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

    /// Load the positions `NDPos_Filename` names. C reads a value holding
    /// `<pos_layout>` as the XML itself and anything else as a file path
    /// (NDPosPluginFileReader.cpp:34-38).
    fn load_filename(&mut self, value: &str) -> Result<usize, String> {
        if value.contains("<pos_layout>") {
            return self.load_positions_xml(value);
        }
        let text = std::fs::read_to_string(value).map_err(|e| format!("{value}: {e}"))?;
        self.load_positions_auto(&text)
    }

    /// Load positions directly.
    fn load_positions(&mut self, positions: Vec<HashMap<String, f64>>) {
        self.append(positions);
    }

    /// C appends a load to the list, leaving the cursor where it is
    /// (`positionArray.insert(positionArray.end(), ...)`, NDPosPlugin.cpp:309).
    fn append(&mut self, positions: Vec<HashMap<String, f64>>) -> usize {
        let count = positions.len();
        self.positions.extend(positions);
        count
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
        self.index = 0;
    }

    /// C `NDPos_CurrentQty`, `positionArray.size()` in both modes.
    fn remaining_positions(&self) -> usize {
        self.positions.len()
    }

    /// C `std::advance(it, index)` in both modes (NDPosPlugin.cpp:145-148).
    fn current_position(&self) -> Option<&HashMap<String, f64>> {
        self.positions.get(self.index)
    }

    /// Whether a position remains to be consumed at the current cursor — C's
    /// `size > 0` (Discard) / `index < size` (Keep) guard
    /// (NDPosPlugin.cpp:99,113).
    fn has_position(&self) -> bool {
        match self.mode {
            PosMode::Discard => !self.positions.is_empty(),
            PosMode::Keep => self.index < self.positions.len(),
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
        ProcessResult::sink(updates)
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
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    /// NDPosPlugin.cpp:203.
    const READ_ATTRIBUTES: bool = false;

    fn bind_attributes(&mut self, attributes: ad_core_rs::plugin::runtime::PluginAttributes) {
        self.attributes = Some(attributes);
    }

    /// C `NDPosPlugin::writeInt32` drops the base class status
    /// (`status = (asynStatus)callParamCallbacks();`, NDPosPlugin.cpp:260): a refused NDPluginDriver write keeps
    /// its clamp and succeeds. Running/Mode/Restart/Delete set their side
    /// params inside the write (:230-250); every NDPos param C reads per
    /// frame is mirrored here, before the next write can read it.
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        _params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        use ad_core_rs::plugin::runtime::WriteOutcome;
        let out = WriteOutcome::from(verdict.succeed());
        let mut state = self.state.lock();
        let s = &mut *state;
        let p = s.params;
        let is = |i: Option<usize>| i == Some(reason);
        let int = |out: WriteOutcome<i32>, i: Option<usize>, v: i32| match i {
            Some(i) => out.set_int32(i, 0, v),
            None => out,
        };
        let empty = |out: WriteOutcome<i32>, i: Option<usize>| match i {
            Some(i) => out.set_octet(i, 0, ""),
            None => out,
        };
        let out = if is(p.running) {
            // :54 `running == NDPOS_RUNNING`; ExpectedID resets on any write.
            s.running = value == 1;
            s.expected_id = s.id_start;
            int(out, p.expected_id, s.id_start)
        } else if is(p.mode) {
            s.mode = match value {
                1 => PosMode::Keep,
                _ => PosMode::Discard,
            };
            s.index = 0;
            int(out, p.current_index, 0)
        } else if is(p.restart) {
            s.index = 0;
            empty(int(out, p.current_index, 0), p.current_pos)
        } else if is(p.delete) {
            s.clear();
            int(
                empty(int(out, p.current_index, 0), p.current_pos),
                p.current_qty,
                0,
            )
        } else if is(p.id_start) {
            s.id_start = value;
            out
        } else if is(p.id_difference) {
            s.id_difference = value;
            out
        } else if is(p.expected_id) {
            s.expected_id = value;
            out
        } else if is(p.missing_frames) {
            s.missing_frames = value;
            out
        } else if is(p.duplicate_frames) {
            s.duplicate_frames = value;
            out
        } else {
            return out;
        };
        out.settled()
    }

    /// C `NDPosPlugin::writeOctet(NDPos_Filename)` loads the positions inside
    /// the write and fails it on invalid XML (NDPosPlugin.cpp:295-315).
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        use ad_core_rs::plugin::runtime::{WriteOutcome, WriteVerdict};
        let mut state = self.state.lock();
        if Some(reason) != state.params.filename {
            return verdict.into();
        }
        let set = |out: WriteOutcome<String>, i: Option<usize>, v: i32| match i {
            Some(i) => out.set_int32(i, 0, v),
            None => out,
        };
        let (valid, qty) = (state.params.file_valid, state.params.current_qty);
        match state.load_filename(value) {
            Ok(_) => {
                let n = state.remaining_positions() as i32;
                set(
                    set(WriteVerdict::Accept(value.to_string()).into(), valid, 1),
                    qty,
                    n,
                )
            }
            Err(_) => set(
                WriteVerdict::Refuse {
                    keep: value.to_string(),
                    message: format!(
                        "NDPosPlugin:writeOctet: status=3, function={reason}, value={value}"
                    ),
                }
                .into(),
                valid,
                0,
            ),
        }
        .settled()
    }

    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
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
                state.duplicate_frames,
            );
            return ProcessResult::sink(updates);
        }

        // Guaranteed `Some` here: has_position() was rechecked above.
        let position = state.current_position().unwrap().clone();

        // C copies the input through the pool and attaches the position
        // attributes to the copy (NDPosPlugin.cpp:142,163).
        let mut out = match pool.alloc_copy(array) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(error = %e, "position output allocation failed; dropping frame");
                return ProcessResult::empty();
            }
        };
        // C `getAttributes` on the copy (NDPosPlugin.cpp:143), before the
        // position attributes and with the counters set so far (:126, :134).
        if let Some(attributes) = &self.attributes {
            let mut pending = Vec::new();
            push_int(
                &mut pending,
                state.params.missing_frames,
                state.missing_frames,
            );
            push_int(
                &mut pending,
                state.params.duplicate_frames,
                state.duplicate_frames,
            );
            push_int(
                &mut pending,
                state.params.current_qty,
                state.remaining_positions() as i32,
            );
            push_int(
                &mut pending,
                state.params.current_index,
                state.current_index_param(),
            );
            attributes.get_attributes(&mut out.attributes, &pending);
        }
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
            state.missing_frames,
        );
        push_int(
            &mut updates,
            state.params.duplicate_frames,
            state.duplicate_frames,
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
            addressed: vec![],
            trace: vec![],
            counted: true,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `status = (asynStatus)callParamCallbacks();` (NDPosPlugin.cpp:260).
    #[test]
    fn a_refused_base_write_succeeds_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let mut base = asyn_rs::port::PortDriverBase::new("T", 1, Default::default());
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        assert_eq!(
            proc.validate_int32(
                0,
                0,
                98,
                clamp,
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Accept(1)
        );
    }
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

        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 1.5).abs() < 1e-10);

        let result = proc.process_array(&Arc::new(make_array(2)), &pool);
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

    /// C `getAttributes` on the copy (NDPosPlugin.cpp:143) comes before the
    /// position attributes (:161-162) and the CurrentIndex step (:190): a
    /// position overrides a file attribute of its name, and a PARAM reads the
    /// index the frame was taken at.
    #[test]
    fn the_plugin_attributes_precede_the_position() {
        use crate::test_util::{publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let proc = PosPluginProcessor::new(PosMode::Keep);
        let positions = [10.0, 20.0].map(|x| HashMap::from([("X".to_string(), x)]));
        proc.load_positions(positions.to_vec());
        proc.start();
        let xml = r#"<Attributes>
            <Attribute name="X" type="CONST" source="file"/>
            <Attribute name="Index" type="PARAM" source="NDPos_CurrentIndex" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) = with_attributes_file("POS_ATTRS", proc, xml);
        let value = |a: &NDArray, name: &str| a.attributes.get(name).map(|a| a.value.clone());
        for (id, x) in [(1, 10.0), (2, 20.0)] {
            publish(&handle, &make_array(id));
            let out = rx.blocking_recv().unwrap();
            assert_eq!(value(&out, "X"), Some(NDAttrValue::Float64(x)));
            assert_eq!(value(&out, "Index"), Some(NDAttrValue::Int32(id - 1)));
        }
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
        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
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

        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
        let x = result.output_arrays[0]
            .attributes
            .get("X")
            .unwrap()
            .value
            .as_f64()
            .unwrap();
        assert!((x - 10.0).abs() < 1e-10);

        let result = proc.process_array(&Arc::new(make_array(2)), &pool);
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
        let result = proc.process_array(&Arc::new(make_array(3)), &pool);
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
        let r1 = proc.process_array(&Arc::new(make_array(1)), &pool);
        assert_eq!(r1.output_arrays.len(), 1);
        // Frame 2 finds no positions: dropped, Running posted IDLE, plugin idle.
        let r2 = proc.process_array(&Arc::new(make_array(2)), &pool);
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

        proc.process_array(&Arc::new(make_array(1)), &pool);

        // Frame 3 (skip frame 2)
        let result = proc.process_array(&Arc::new(make_array(3)), &pool);
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

        proc.process_array(&Arc::new(make_array(1)), &pool);

        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
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
        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
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

        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
        let attrs = &result.output_arrays[0].attributes;
        assert!((attrs.get("x").unwrap().value.as_f64().unwrap() - 10.0).abs() < 1e-10);
        assert!((attrs.get("y").unwrap().value.as_f64().unwrap() - 100.0).abs() < 1e-10);

        let result = proc.process_array(&Arc::new(make_array(2)), &pool);
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
        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
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

    /// NDPosPlugin.cpp:301-313: a valid file sets `NDPos_FileValid` 1 and
    /// `NDPos_CurrentQty`, an invalid one `NDPos_FileValid` 0 and
    /// `status = asynError`.
    #[test]
    fn filename_loads_inside_the_write_as_c_write_octet_does() {
        use ad_core_rs::plugin::runtime::{SideParam, WriteVerdict};
        let mut proc = PosPluginProcessor::new(PosMode::Discard);
        let mut base = asyn_rs::port::PortDriverBase::new("T", 1, Default::default());
        proc.register_params(&mut base).unwrap();
        let (file, valid, qty) = {
            let p = &proc.state.lock().params;
            (
                p.filename.unwrap(),
                p.file_valid.unwrap(),
                p.current_qty.unwrap(),
            )
        };
        let write =
            |v: &str| proc.validate_octet(file, 0, v, WriteVerdict::Accept(v.to_string()), &base);
        let side = |i: usize, v: i32| SideParam::Int32 {
            reason: i,
            addr: 0,
            value: v,
        };

        let xml = r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions><position x="1"/><position x="2"/></positions></pos_layout>"#;
        let out = write(xml);
        assert_eq!(out.verdict, WriteVerdict::Accept(xml.to_string()));
        assert_eq!(out.side, vec![side(valid, 1), side(qty, 2)]);
        assert!(out.settled);
        assert_eq!(proc.remaining_positions(), 2);

        let bad = "/nonexistent/positions.xml";
        let out = write(bad);
        assert_eq!(
            out.verdict,
            WriteVerdict::Refuse {
                keep: bad.to_string(),
                message: format!("NDPosPlugin:writeOctet: status=3, function={file}, value={bad}"),
            }
        );
        assert_eq!(out.side, vec![side(valid, 0)]);
        assert_eq!(proc.remaining_positions(), 2);
    }

    /// NDPosPlugin.cpp:309 `positionArray.insert(positionArray.end(), ...)`
    /// and :185 `positionArray.erase(positionArray.begin())`: one list, so a
    /// load appends and Keep never sees what Discard consumed.
    #[test]
    fn positions_are_one_list_as_c_position_array_is() {
        let at = |x: f64| HashMap::from([("x".to_string(), x)]);
        let mut state = PosPluginState::new(PosMode::Discard);
        state.load_positions(vec![at(1.0), at(2.0)]);
        state.load_positions(vec![at(3.0)]);
        assert_eq!(state.remaining_positions(), 3);
        state.advance();
        state.mode = PosMode::Keep;
        assert_eq!(state.remaining_positions(), 2);
        assert_eq!(state.current_position(), Some(&at(2.0)));
        state.advance();
        state.load_positions(vec![at(4.0)]);
        assert_eq!(state.current_position(), Some(&at(3.0)));
    }

    /// NDPosPluginFileReader.cpp:34-38: `if (filename.find("<pos_layout>") !=
    /// std::string::npos)` reads from memory, else `xmlReaderForFile`.
    #[test]
    fn filename_without_pos_layout_names_a_file_as_c_validate_xml_does() {
        let dir = std::env::temp_dir().join(format!("ndpos-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("positions.xml");
        std::fs::write(
            &path,
            r#"<pos_layout><dimensions><dimension name="x"/></dimensions><positions><position x="1"/><position x="2"/><position x="3"/></positions></pos_layout>"#,
        )
        .unwrap();
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let loaded = proc.state.lock().load_filename(path.to_str().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded, Ok(3));
        assert_eq!(proc.remaining_positions(), 3);
    }

    fn pos_port() -> (PosPluginProcessor, asyn_rs::port::PortDriverBase) {
        let mut proc = PosPluginProcessor::new(PosMode::Keep);
        let mut base = asyn_rs::port::PortDriverBase::new("T", 1, Default::default());
        proc.register_params(&mut base).unwrap();
        (proc, base)
    }

    fn pos_write(
        proc: &PosPluginProcessor,
        base: &mut asyn_rs::port::PortDriverBase,
        name: &str,
        value: i32,
    ) -> Vec<ad_core_rs::plugin::runtime::SideParam> {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let reason = base.find_param(name).unwrap();
        let out = proc.validate_int32(
            reason,
            0,
            value,
            WriteVerdict::Accept(value),
            &mut ad_core_rs::plugin::runtime::WritePort::new(base),
        );
        assert!(out.settled);
        out.side
    }

    /// NDPosPlugin.cpp:230-250: each write sets its side params inside the
    /// write, and Running resets ExpectedID to IDStart on stop as well.
    #[test]
    fn writes_set_their_params_inside_the_write_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::SideParam;
        let (proc, mut base) = pos_port();
        let ids = [
            "NDPos_CurrentPos",
            "NDPos_CurrentIndex",
            "NDPos_CurrentQty",
            "NDPos_ExpectedID",
        ]
        .map(|n| (n, base.find_param(n).unwrap()));
        let id = |n: &str| ids.iter().find(|(m, _)| *m == n).unwrap().1;
        let int = |n: &str, value| SideParam::Int32 {
            reason: id(n),
            addr: 0,
            value,
        };
        let pos = SideParam::Octet {
            reason: id("NDPos_CurrentPos"),
            addr: 0,
            value: String::new(),
        };
        assert!(pos_write(&proc, &mut base, "NDPos_IDStart", 7).is_empty());
        assert_eq!(
            pos_write(&proc, &mut base, "NDPos_Running", 1),
            [int("NDPos_ExpectedID", 7)]
        );
        assert!(proc.state.lock().running);
        assert_eq!(
            pos_write(&proc, &mut base, "NDPos_Running", 0),
            [int("NDPos_ExpectedID", 7)]
        );
        assert!(!proc.state.lock().running);
        pos_write(&proc, &mut base, "NDPos_Running", 2);
        assert!(!proc.state.lock().running, ":54 runs only on NDPOS_RUNNING");
        assert_eq!(
            pos_write(&proc, &mut base, "NDPos_Mode", 1),
            [int("NDPos_CurrentIndex", 0)]
        );
        assert_eq!(
            pos_write(&proc, &mut base, "NDPos_Restart", 1),
            [int("NDPos_CurrentIndex", 0), pos.clone()]
        );
        proc.load_positions(vec![HashMap::new()]);
        assert_eq!(
            pos_write(&proc, &mut base, "NDPos_Delete", 1),
            [
                int("NDPos_CurrentIndex", 0),
                pos,
                int("NDPos_CurrentQty", 0)
            ]
        );
        assert_eq!(proc.remaining_positions(), 0);
        pos_write(&proc, &mut base, "NDPos_MissingFrames", 0);
        assert_eq!(proc.missing_frames(), 0);
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
        let result = proc.process_array(&Arc::new(make_array(1)), &pool);
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
        let result = proc.process_array(&Arc::new(make_array(3)), &pool);
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
        proc.process_array(&Arc::new(make_array(1)), &pool);
        proc.process_array(&Arc::new(make_array(3)), &pool);
        let r = proc.process_array(&Arc::new(make_array(5)), &pool);
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

    #[test]
    fn pos_output_comes_from_the_pool_and_is_reused() {
        let proc = PosPluginProcessor::new(PosMode::Discard);
        let mut pos = HashMap::new();
        pos.insert("X".into(), 1.5);
        proc.load_positions(vec![pos.clone(), pos]);
        proc.start();
        let pool = NDArrayPool::new(0);

        let first = proc
            .process_array(&Arc::new(make_array(1)), &pool)
            .output_arrays
            .remove(0);
        assert_eq!(first.pool_id(), pool.id());
        assert!(first.attributes.get("X").is_some());
        let ptr = first.data.as_u8_slice().as_ptr();
        drop(first);

        let second = proc
            .process_array(&Arc::new(make_array(2)), &pool)
            .output_arrays
            .remove(0);
        assert_eq!(
            second.data.as_u8_slice().as_ptr(),
            ptr,
            "the second frame reuses the freed buffer"
        );
        assert_eq!(pool.num_alloc_buffers(), 1);
    }
}
