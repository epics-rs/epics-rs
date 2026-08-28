//! NDPluginAttrPlot: caches numeric NDArray attribute values over an
//! acquisition and exposes selected ones as waveform records.
//!
//! Port of ADCore `NDPluginAttrPlot`. The C++ model separates two counts:
//!
//! * `n_attributes` — the maximum number of *tracked* numeric attributes.
//!   Attribute names are discovered from the first frame of an acquisition
//!   (and re-discovered after a reset), sorted, and capped to `n_attributes`.
//!   One circular buffer per tracked attribute.
//! * `n_data_blocks` — the number of *waveform outputs* (asyn addresses).
//!   Each data block has an independent `DataSelect` value that maps it to a
//!   tracked attribute index, or to the special UID buffer (`-1`), or to
//!   nothing (`-2`).
//!
//! `DataLabel` is the human-readable name of the attribute a block is bound
//! to; `NPts` is the current number of cached points. The waveform emitted
//! for a block is padded out to `cache_size` with the last valid point to
//! avoid plot artifacts (C++ `callback_data`).
//!
//! The two callback paths are separate in C and separate here. `AP_NPts` and
//! the attribute/label/selection metadata are posted from the frame path
//! (C++ `processCallbacks`, `NDPluginAttrPlot.cpp:126-146`); `AP_Data` is
//! posted only from `callback_data` (`:96-124`), which C++ drives from
//! `ExposeDataTask::run` once per `ND_ATTRPLOT_DATA_EXPOSURE_PERIOD` (1 s,
//! `NDPluginAttrPlot.h:39`) and once per `DataSelect` write (`:283-289`).
//! Posting the waveform per frame would make a `camonitor` on `AP_Data` fire
//! at the detector frame rate.

use std::collections::VecDeque;

/// C++ `ND_ATTRPLOT_DATA_EXPOSURE_PERIOD` (`NDPluginAttrPlot.h:39`) — the
/// period of `ExposeDataTask::run`'s `AP_Data` post.
const ATTRPLOT_DATA_EXPOSURE_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, ParamUpdate, PluginParamSnapshot, ProcessResult,
};
use parking_lot::Mutex;

/// `DataSelect` value meaning "this block plots the UID buffer".
pub const ATTRPLOT_UID_INDEX: i32 = -1;
/// `DataSelect` value meaning "this block plots nothing".
pub const ATTRPLOT_NONE_INDEX: i32 = -2;
/// `DataLabel` text for the UID buffer.
pub const ATTRPLOT_UID_LABEL: &str = "UID";
/// `DataLabel` text for an unbound block.
pub const ATTRPLOT_NONE_LABEL: &str = "None";

/// Processor that tracks attribute values over time in circular buffers.
pub struct AttrPlotProcessor {
    state: Mutex<AttrPlotState>,
}

/// Everything one frame mutates. A frame resets, rebuilds, pushes and posts in
/// one pass, so the whole set moves under a single lock.
struct AttrPlotState {
    /// Maximum number of tracked attributes (C++ `n_attributes_`).
    n_attributes: usize,
    /// Number of waveform output blocks (C++ `n_data_blocks_`).
    n_data_blocks: usize,
    /// Cache size per buffer; `0` means unlimited.
    cache_size: usize,
    /// Tracked attribute names (sorted, length <= `n_attributes`).
    attributes: Vec<String>,
    /// One circular buffer per tracked attribute.
    buffers: Vec<VecDeque<f64>>,
    /// Circular buffer of unique_id values.
    uid_buffer: VecDeque<f64>,
    /// Per-block attribute selection: index into `attributes`, or one of the
    /// `ATTRPLOT_UID_INDEX` / `ATTRPLOT_NONE_INDEX` sentinels.
    data_selections: Vec<i32>,
    /// Whether attributes have been discovered for the current acquisition.
    initialized: bool,
    /// The unique_id from the last processed frame.
    last_uid: i32,
    /// When `AP_Data` was last posted. `None` until the first exposure, which
    /// is why the first frame of an acquisition always exposes.
    last_expose: Option<std::time::Instant>,
    /// Param indices (set after registration).
    params: AttrPlotParams,
}

/// Param reasons resolved after `register_params`.
#[derive(Default)]
struct AttrPlotParams {
    /// `AP_Data` — Float64Array, addressed by data block.
    data: Option<usize>,
    /// `AP_DataLabel` — Octet, addressed by data block.
    data_label: Option<usize>,
    /// `AP_DataSelect` — Int32, addressed by data block.
    data_select: Option<usize>,
    /// `AP_Attribute` — Octet, addressed by attribute index.
    attribute: Option<usize>,
    /// `AP_Reset` — Int32.
    reset: Option<usize>,
    /// `AP_NPts` — Int32.
    npts: Option<usize>,
}

impl AttrPlotProcessor {
    /// Create a processor.
    ///
    /// * `n_attributes` — maximum tracked attributes.
    /// * `cache_size` — per-buffer cache size (`0` = unlimited).
    /// * `n_data_blocks` — number of waveform output blocks.
    pub fn new(n_attributes: usize, cache_size: usize, n_data_blocks: usize) -> Self {
        Self {
            state: Mutex::new(AttrPlotState::new(n_attributes, cache_size, n_data_blocks)),
        }
    }

    /// Get the list of tracked attribute names.
    pub fn attributes(&self) -> Vec<String> {
        self.state.lock().attributes.clone()
    }

    /// Get the circular buffer for a specific attribute index.
    pub fn buffer(&self, index: usize) -> Option<VecDeque<f64>> {
        self.state.lock().buffers.get(index).cloned()
    }

    /// Get the unique_id buffer.
    pub fn uid_buffer(&self) -> VecDeque<f64> {
        self.state.lock().uid_buffer.clone()
    }

    /// Get the number of tracked attributes.
    pub fn num_attributes(&self) -> usize {
        self.state.lock().attributes.len()
    }

    /// Get the number of waveform output blocks.
    pub fn num_data_blocks(&self) -> usize {
        self.state.lock().n_data_blocks
    }

    /// Find the index of a named attribute. Returns `None` if not tracked.
    pub fn find_attribute(&self, name: &str) -> Option<usize> {
        self.state.lock().find_attribute(name)
    }

    /// Current `DataSelect` value for a block.
    pub fn data_select(&self, block: usize) -> Option<i32> {
        self.state.lock().data_select(block)
    }

    /// Bind a data block to an attribute index (or a UID/NONE sentinel).
    pub fn set_data_select(&self, block: usize, value: i32) -> Result<(), &'static str> {
        self.state.lock().set_data_select(block, value)
    }

    /// The `DataLabel` text for a block, derived from its `DataSelect`.
    pub fn data_label(&self, block: usize) -> String {
        self.state.lock().data_label(block)
    }

    /// Reset all buffers; the next frame re-discovers attributes.
    pub fn reset(&self) {
        self.state.lock().reset();
    }
}

impl AttrPlotState {
    fn new(n_attributes: usize, cache_size: usize, n_data_blocks: usize) -> Self {
        Self {
            n_attributes,
            n_data_blocks,
            cache_size,
            attributes: Vec::new(),
            buffers: Vec::new(),
            uid_buffer: VecDeque::new(),
            data_selections: vec![ATTRPLOT_NONE_INDEX; n_data_blocks],
            initialized: false,
            last_uid: -1,
            last_expose: None,
            params: AttrPlotParams::default(),
        }
    }

    /// Find the index of a named attribute. Returns `None` if not tracked.
    fn find_attribute(&self, name: &str) -> Option<usize> {
        self.attributes.iter().position(|n| n == name)
    }

    /// Current `DataSelect` value for a block.
    fn data_select(&self, block: usize) -> Option<i32> {
        self.data_selections.get(block).copied()
    }

    /// Bind a data block to an attribute index (or a UID/NONE sentinel).
    ///
    /// Mirrors C++ `writeInt32(NDAttrPlotDataSelect)`: rejects out-of-range
    /// blocks and selections that point past the tracked attributes.
    fn set_data_select(&mut self, block: usize, value: i32) -> Result<(), &'static str> {
        if block >= self.n_data_blocks {
            return Err("data block index out of range");
        }
        // C rejects only a strictly positive selection past the end; value 0 is
        // always accepted, even with no tracked attributes (NDPluginAttrPlot.cpp:283).
        if value > 0 && (value as usize) >= self.attributes.len() {
            return Err("attribute selection out of range");
        }
        self.data_selections[block] = value;
        Ok(())
    }

    /// The `DataLabel` text for a block, derived from its `DataSelect`.
    fn data_label(&self, block: usize) -> String {
        match self.data_selections.get(block).copied() {
            Some(ATTRPLOT_UID_INDEX) => ATTRPLOT_UID_LABEL.to_string(),
            Some(sel) if sel >= 0 && (sel as usize) < self.attributes.len() => {
                self.attributes[sel as usize].clone()
            }
            _ => ATTRPLOT_NONE_LABEL.to_string(),
        }
    }

    /// Reset all buffers; the next frame re-discovers attributes.
    fn reset(&mut self) {
        self.initialized = false;
        self.uid_buffer.clear();
        for buf in &mut self.buffers {
            buf.clear();
        }
        self.last_uid = -1;
    }

    /// Push a value into a ring buffer, enforcing `cache_size`.
    fn push_capped(buf: &mut VecDeque<f64>, value: f64, cache_size: usize) {
        if cache_size > 0 && buf.len() >= cache_size {
            buf.pop_front();
        }
        buf.push_back(value);
    }

    /// Discover the tracked attributes from a frame (C++ `rebuild_attributes`).
    ///
    /// Existing block selections are preserved by attribute *name*: a block
    /// that pointed at "Temp" before the rebuild still points at "Temp"
    /// afterwards (or `NONE` if "Temp" is no longer present).
    fn rebuild_attributes(&mut self, array: &NDArray) {
        // Remember each block's current selection by name.
        let prior: Vec<Option<String>> = self
            .data_selections
            .iter()
            .map(|&sel| match sel {
                ATTRPLOT_UID_INDEX => Some(ATTRPLOT_UID_LABEL.to_string()),
                s if s >= 0 && (s as usize) < self.attributes.len() => {
                    Some(self.attributes[s as usize].clone())
                }
                _ => None,
            })
            .collect();

        let mut names: Vec<String> = Vec::new();
        for attr in array.attributes.iter() {
            if attr.value.as_f64().is_some() {
                names.push(attr.name.clone());
            }
        }
        names.sort();
        names.truncate(self.n_attributes);

        self.buffers = vec![VecDeque::new(); names.len()];
        self.attributes = names;

        // Re-resolve each block selection against the new attribute list.
        for (i, want) in prior.into_iter().enumerate() {
            self.data_selections[i] = match want {
                Some(ref n) if n == ATTRPLOT_UID_LABEL => ATTRPLOT_UID_INDEX,
                Some(n) => self
                    .attributes
                    .iter()
                    .position(|a| a == &n)
                    .map(|p| p as i32)
                    .unwrap_or(ATTRPLOT_NONE_INDEX),
                None => ATTRPLOT_NONE_INDEX,
            };
        }
        self.initialized = true;
    }

    /// Push the current frame's attribute values into the buffers.
    fn push_data(&mut self, array: &NDArray) {
        Self::push_capped(
            &mut self.uid_buffer,
            array.unique_id as f64,
            self.cache_size,
        );
        for (i, name) in self.attributes.iter().enumerate() {
            let value = array
                .attributes
                .get(name)
                .and_then(|attr| attr.value.as_f64())
                .unwrap_or(f64::NAN);
            Self::push_capped(&mut self.buffers[i], value, self.cache_size);
        }
    }

    /// Build the padded waveform for one data block (C++ `callback_data`).
    ///
    /// Returns the values padded to `cache_size` (or to the current point
    /// count when `cache_size` is unlimited) with the last valid point.
    fn block_waveform(&self, block: usize) -> Vec<f64> {
        let selected = self
            .data_selections
            .get(block)
            .copied()
            .unwrap_or(ATTRPLOT_NONE_INDEX);
        let src: Option<&VecDeque<f64>> = match selected {
            ATTRPLOT_UID_INDEX => Some(&self.uid_buffer),
            s if s >= 0 && (s as usize) < self.buffers.len() => Some(&self.buffers[s as usize]),
            _ => None,
        };
        let size = self.uid_buffer.len();
        // Target length: the fixed cache size, or the live count if unlimited.
        let target = if self.cache_size > 0 {
            self.cache_size
        } else {
            size
        };
        let mut out: Vec<f64> = match src {
            Some(buf) => buf.iter().copied().collect(),
            None => vec![f64::NAN; size],
        };
        // Pad the tail with the last valid point to suppress plot artifacts.
        let pad = out.last().copied().unwrap_or(f64::NAN);
        if out.len() < target {
            out.resize(target, pad);
        } else {
            out.truncate(target);
        }
        out
    }

    /// The `AP_Data` waveforms — C++ `callback_data`
    /// (`NDPluginAttrPlot.cpp:96-124`). Posted on the exposure period and on a
    /// `DataSelect` write, never per frame.
    fn data_updates(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        if let Some(data) = self.params.data {
            for block in 0..self.n_data_blocks {
                updates.push(ParamUpdate::float64_array_addr(
                    data,
                    block as i32,
                    self.block_waveform(block),
                ));
            }
        }
        updates
    }

    /// Whether the exposure period has elapsed, arming the next one when it
    /// has. C++ posts from a 1 Hz thread; without a periodic hook in the
    /// plugin runtime the frame path carries the clock, so the cadence is
    /// "at most once per period" rather than "once per period even with no
    /// frames".
    fn expose_due(&mut self) -> bool {
        let now = std::time::Instant::now();
        let due = match self.last_expose {
            None => true,
            Some(prev) => now.duration_since(prev) >= ATTRPLOT_DATA_EXPOSURE_PERIOD,
        };
        if due {
            self.last_expose = Some(now);
        }
        due
    }

    /// The metadata + point-count updates — C++ `callback_attributes`,
    /// `callback_selected` and the `NDAttrPlotNPts` write in
    /// `processCallbacks`.
    fn meta_updates(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        if let Some(label) = self.params.data_label {
            for block in 0..self.n_data_blocks {
                updates.push(ParamUpdate::octet_addr(
                    label,
                    block as i32,
                    self.data_label(block),
                ));
            }
        }
        if let Some(select) = self.params.data_select {
            for block in 0..self.n_data_blocks {
                updates.push(ParamUpdate::int32_addr(
                    select,
                    block as i32,
                    self.data_selections[block],
                ));
            }
        }
        // Per-attribute name.
        if let Some(attribute) = self.params.attribute {
            for i in 0..self.n_attributes {
                let name = self.attributes.get(i).cloned().unwrap_or_default();
                updates.push(ParamUpdate::octet_addr(attribute, i as i32, name));
            }
        }
        if let Some(npts) = self.params.npts {
            updates.push(ParamUpdate::int32(npts, self.uid_buffer.len() as i32));
        }
        updates
    }

    /// Metadata plus an unconditional `AP_Data` post — the shape C++ produces
    /// for a `DataSelect` write (`callback_selected` then `callback_data`,
    /// `NDPluginAttrPlot.cpp:286-287`). Re-arms the exposure clock so the
    /// forced post also counts as this period's exposure.
    fn build_updates(&mut self) -> Vec<ParamUpdate> {
        self.last_expose = Some(std::time::Instant::now());
        let mut updates = self.data_updates();
        updates.extend(self.meta_updates());
        updates
    }
}

impl NDPluginProcess for AttrPlotProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let mut state = self.state.lock();

        // Re-acquisition: a UID at or below the last cached one resets.
        if !state.uid_buffer.is_empty() && array.unique_id <= state.last_uid {
            state.reset();
        }
        state.last_uid = array.unique_id;

        if !state.initialized {
            state.rebuild_attributes(array);
        }
        state.push_data(array);

        // C++ `processCallbacks` posts NPts and the metadata; `AP_Data` comes
        // from the exposure path only (ADP-53).
        let mut updates = state.meta_updates();
        if state.expose_due() {
            updates.extend(state.data_updates());
        }
        ProcessResult::sink(updates)
    }

    fn plugin_type(&self) -> &str {
        // C sets PluginType to "NDAttrPlot" (NDPluginAttrPlot.cpp:87), not the
        // class name.
        "NDAttrPlot"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        let state = self.state.get_mut();
        base.create_param("AP_Data", ParamType::Float64Array)?;
        base.create_param("AP_DataLabel", ParamType::Octet)?;
        base.create_param("AP_DataSelect", ParamType::Int32)?;
        base.create_param("AP_Attribute", ParamType::Octet)?;
        base.create_param("AP_Reset", ParamType::Int32)?;
        base.create_param("AP_NPts", ParamType::Int32)?;

        state.params.data = base.find_param("AP_Data");
        state.params.data_label = base.find_param("AP_DataLabel");
        state.params.data_select = base.find_param("AP_DataSelect");
        state.params.attribute = base.find_param("AP_Attribute");
        state.params.reset = base.find_param("AP_Reset");
        state.params.npts = base.find_param("AP_NPts");
        Ok(())
    }

    fn on_param_change(&self, reason: usize, params: &PluginParamSnapshot) -> ParamChangeResult {
        let mut state = self.state.lock();
        if Some(reason) == state.params.data_select {
            let block = params.addr as usize;
            let value = params.value.as_i32();
            if state.set_data_select(block, value).is_ok() {
                // Re-emit label + waveform for the rebound block.
                return ParamChangeResult::updates(state.build_updates());
            }
        } else if Some(reason) == state.params.reset {
            // C calls reset_data() on ANY write to the reset param — there is no
            // value test (NDPluginAttrPlot.cpp:290-292).
            state.reset();
            return ParamChangeResult::updates(state.build_updates());
        }
        ParamChangeResult::updates(vec![])
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
            arr.attributes.add(NDAttribute::new_static(
                *name,
                String::new(),
                NDAttrSource::Driver,
                NDAttrValue::Float64(*value),
            ));
        }
        arr
    }

    /// Register the AttrPlot params on a scratch port so `params.data` is
    /// resolved and `AP_Data` updates are actually built.
    fn proc_with_params(n_attributes: usize, cache: usize, blocks: usize) -> AttrPlotProcessor {
        let mut proc = AttrPlotProcessor::new(n_attributes, cache, blocks);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "ATTRPLOT_ADP53",
            blocks.max(n_attributes),
            asyn_rs::port::PortFlags::default(),
        );
        proc.register_params(&mut base).unwrap();
        proc
    }

    fn has_data_update(result: &ProcessResult, reason: usize) -> bool {
        result
            .param_updates
            .iter()
            .any(|u| matches!(u, ParamUpdate::Float64Array { reason: r, .. } if *r == reason))
    }

    /// ADP-53: `AP_Data` is posted on the exposure period, not per frame.
    /// C++ drives `callback_data` from `ExposeDataTask::run` every
    /// `ND_ATTRPLOT_DATA_EXPOSURE_PERIOD`; `processCallbacks` posts only
    /// `NDAttrPlotNPts` (`NDPluginAttrPlot.cpp:126-146`). Three frames
    /// processed back to back fall inside one period, so exactly one of them
    /// may carry the waveform.
    #[test]
    fn test_adp53_ap_data_is_rate_limited_to_the_exposure_period() {
        let proc = proc_with_params(8, 100, 4);
        let data_reason = proc.state.lock().params.data.expect("AP_Data registered");
        let npts_reason = proc.state.lock().params.npts.expect("AP_NPts registered");
        let pool = NDArrayPool::new(1_000_000);

        let mut exposures = 0;
        for uid in 1..=3 {
            let arr = make_array_with_attrs(uid, &[("Temp", 25.0 + uid as f64)]);
            let result = proc.process_array(&arr, &pool);
            if has_data_update(&result, data_reason) {
                exposures += 1;
            }
            // NPts is the frame-path post and must be on every frame, as
            // C++ `processCallbacks` writes it every call.
            assert!(
                result.param_updates.iter().any(|u| matches!(
                    u,
                    ParamUpdate::Int32 { reason: r, .. } if *r == npts_reason
                )),
                "frame {uid} must still post AP_NPts",
            );
        }
        assert_eq!(
            exposures, 1,
            "three frames inside one exposure period must post AP_Data once",
        );
    }

    /// ADP-53: a `DataSelect` write posts `AP_Data` regardless of the clock —
    /// C++ `writeInt32` calls `callback_selected()` then `callback_data()`
    /// (`NDPluginAttrPlot.cpp:283-289`).
    #[test]
    fn test_adp53_data_select_write_exposes_immediately() {
        let proc = proc_with_params(8, 100, 4);
        let data_reason = proc.state.lock().params.data.expect("AP_Data registered");
        let select_reason = proc
            .state
            .lock()
            .params
            .data_select
            .expect("AP_DataSelect registered");
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_array_with_attrs(1, &[("Temp", 25.0)]);
        proc.process_array(&arr, &pool); // consumes this period's exposure

        let snapshot = PluginParamSnapshot {
            enable_callbacks: true,
            reason: select_reason,
            addr: 0,
            value: ad_core_rs::plugin::runtime::ParamChangeValue::Int32(0),
        };
        let result = proc.on_param_change(select_reason, &snapshot);
        assert!(
            result.param_updates.iter().any(|u| {
                matches!(u, ParamUpdate::Float64Array { reason: r, .. } if *r == data_reason)
            }),
            "a DataSelect write must post AP_Data even inside the period",
        );
    }

    #[test]
    fn test_attribute_auto_detection() {
        let proc = AttrPlotProcessor::new(8, 100, 4);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = make_array_with_attrs(1, &[("Temp", 25.0), ("Gain", 1.5)]);
        arr.attributes.add(NDAttribute::new_static(
            "Label",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::String("test".to_string()),
        ));
        proc.process_array(&arr, &pool);

        assert_eq!(proc.num_attributes(), 2);
        assert_eq!(proc.attributes()[0], "Gain");
        assert_eq!(proc.attributes()[1], "Temp");
    }

    #[test]
    fn test_n_attributes_caps_tracked_count() {
        // n_attributes = 2: only the first 2 (sorted) attributes are tracked.
        let proc = AttrPlotProcessor::new(2, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(1, &[("D", 4.0), ("A", 1.0), ("C", 3.0), ("B", 2.0)]);
        proc.process_array(&arr, &pool);
        assert_eq!(proc.num_attributes(), 2);
        assert_eq!(proc.attributes(), vec!["A", "B"]);
    }

    #[test]
    fn test_data_select_maps_block_to_attribute() {
        // 3 attributes, 2 data blocks. Block 0 -> "B" (idx 1), block 1 -> UID.
        let proc = AttrPlotProcessor::new(8, 100, 2);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(1, &[("A", 10.0), ("B", 20.0), ("C", 30.0)]);
        proc.process_array(&arr, &pool);

        proc.set_data_select(0, 1).unwrap(); // "B"
        proc.set_data_select(1, ATTRPLOT_UID_INDEX).unwrap();

        assert_eq!(proc.data_label(0), "B");
        assert_eq!(proc.data_label(1), ATTRPLOT_UID_LABEL);

        let wf0 = proc.state.lock().block_waveform(0);
        assert!((wf0[0] - 20.0).abs() < 1e-10, "block 0 plots attribute B");
        let wf1 = proc.state.lock().block_waveform(1);
        assert!((wf1[0] - 1.0).abs() < 1e-10, "block 1 plots UID");
    }

    #[test]
    fn test_data_select_rejects_out_of_range() {
        let proc = AttrPlotProcessor::new(8, 100, 2);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(1, &[("A", 1.0)]);
        proc.process_array(&arr, &pool);

        // Only 1 attribute -> selection 1 is out of range.
        assert!(proc.set_data_select(0, 1).is_err());
        // Block 5 does not exist.
        assert!(proc.set_data_select(5, 0).is_err());
        // Valid: attribute 0 and the UID sentinel.
        assert!(proc.set_data_select(0, 0).is_ok());
        assert!(proc.set_data_select(1, ATTRPLOT_UID_INDEX).is_ok());
    }

    #[test]
    fn test_data_select_zero_accepted_with_no_attributes() {
        // C accepts DataSelect 0 before any frame, even with no tracked
        // attributes (the reject is `value > 0`, NDPluginAttrPlot.cpp:283).
        let proc = AttrPlotProcessor::new(8, 100, 2);
        assert!(proc.attributes().is_empty());
        assert!(proc.set_data_select(0, 0).is_ok());
        assert_eq!(proc.state.lock().data_selections[0], 0);
    }

    #[test]
    fn test_unbound_block_label_is_none() {
        let proc = AttrPlotProcessor::new(8, 100, 3);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(1, &[("A", 1.0)]);
        proc.process_array(&arr, &pool);
        // Block 2 was never selected.
        assert_eq!(proc.data_label(2), ATTRPLOT_NONE_LABEL);
        assert_eq!(proc.data_select(2), Some(ATTRPLOT_NONE_INDEX));
    }

    #[test]
    fn test_npts_tracks_point_count() {
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=4 {
            let arr = make_array_with_attrs(i, &[("X", i as f64)]);
            proc.process_array(&arr, &pool);
        }
        assert_eq!(proc.uid_buffer().len(), 4);
    }

    #[test]
    fn test_waveform_padded_to_cache_size() {
        // cache_size = 6, only 3 points pushed -> waveform padded to 6 with
        // the last point.
        let proc = AttrPlotProcessor::new(8, 6, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=3 {
            let arr = make_array_with_attrs(i, &[("X", i as f64 * 10.0)]);
            proc.process_array(&arr, &pool);
        }
        proc.set_data_select(0, 0).unwrap();
        let wf = proc.state.lock().block_waveform(0);
        assert_eq!(wf.len(), 6);
        assert!((wf[0] - 10.0).abs() < 1e-10);
        assert!((wf[2] - 30.0).abs() < 1e-10);
        // Tail padded with the last point (30.0).
        assert!((wf[3] - 30.0).abs() < 1e-10);
        assert!((wf[5] - 30.0).abs() < 1e-10);
    }

    #[test]
    fn test_data_select_preserved_across_rebuild() {
        // Bind block 0 to "Temp", then re-acquire (UID resets). After the
        // rebuild block 0 must still point at "Temp".
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(5, &[("Gain", 1.0), ("Temp", 25.0)]);
        proc.process_array(&arr, &pool);
        let temp_idx = proc.find_attribute("Temp").unwrap() as i32;
        proc.set_data_select(0, temp_idx).unwrap();

        // Re-acquisition (UID drops); same attributes.
        let arr2 = make_array_with_attrs(1, &[("Gain", 2.0), ("Temp", 99.0)]);
        proc.process_array(&arr2, &pool);
        assert_eq!(proc.data_label(0), "Temp");
        let wf = proc.state.lock().block_waveform(0);
        assert!((wf[0] - 99.0).abs() < 1e-10);
    }

    #[test]
    fn test_value_tracking() {
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=5 {
            let arr = make_array_with_attrs(i, &[("Value", i as f64 * 10.0)]);
            proc.process_array(&arr, &pool);
        }
        let idx = proc.find_attribute("Value").unwrap();
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 5);
        assert!((buf[0] - 10.0).abs() < 1e-10);
        assert!((buf[4] - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_circular_buffer_cache_size() {
        let proc = AttrPlotProcessor::new(8, 3, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=5 {
            let arr = make_array_with_attrs(i, &[("Val", i as f64)]);
            proc.process_array(&arr, &pool);
        }
        let idx = proc.find_attribute("Val").unwrap();
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 3);
        assert!((buf[0] - 3.0).abs() < 1e-10);
        assert!((buf[2] - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_uid_decrease_resets_buffers() {
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=5 {
            let arr = make_array_with_attrs(i, &[("X", i as f64)]);
            proc.process_array(&arr, &pool);
        }
        let idx = proc.find_attribute("X").unwrap();
        assert_eq!(proc.buffer(idx).unwrap().len(), 5);

        let arr = make_array_with_attrs(1, &[("X", 100.0)]);
        proc.process_array(&arr, &pool);
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 1);
        assert!((buf[0] - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_missing_attribute_uses_nan() {
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        let arr1 = make_array_with_attrs(1, &[("Temp", 25.0)]);
        proc.process_array(&arr1, &pool);

        let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr2.unique_id = 2;
        proc.process_array(&arr2, &pool);

        let idx = proc.find_attribute("Temp").unwrap();
        let buf = proc.buffer(idx).unwrap();
        assert_eq!(buf.len(), 2);
        assert!((buf[0] - 25.0).abs() < 1e-10);
        assert!(buf[1].is_nan());
    }

    #[test]
    fn test_manual_reset() {
        let proc = AttrPlotProcessor::new(8, 100, 1);
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_array_with_attrs(5, &[("A", 1.0), ("B", 2.0)]);
        proc.process_array(&arr, &pool);
        assert_eq!(proc.num_attributes(), 2);

        proc.reset();
        // Re-initializes from the next frame.
        let arr2 = make_array_with_attrs(1, &[("C", 3.0)]);
        proc.process_array(&arr2, &pool);
        assert_eq!(proc.num_attributes(), 1);
        assert_eq!(proc.attributes()[0], "C");
    }

    #[test]
    fn test_unlimited_buffer() {
        let proc = AttrPlotProcessor::new(8, 0, 1);
        let pool = NDArrayPool::new(1_000_000);
        for i in 1..=100 {
            let arr = make_array_with_attrs(i, &[("X", i as f64)]);
            proc.process_array(&arr, &pool);
        }
        let idx = proc.find_attribute("X").unwrap();
        assert_eq!(proc.buffer(idx).unwrap().len(), 100);
    }

    #[test]
    fn test_plugin_type() {
        // C PluginType is "NDAttrPlot" (NDPluginAttrPlot.cpp:87).
        let proc = AttrPlotProcessor::new(8, 100, 1);
        assert_eq!(proc.plugin_type(), "NDAttrPlot");
    }
}
