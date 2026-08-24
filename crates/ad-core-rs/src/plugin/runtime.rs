//! Plugin runtime: control plane (PortActor) + data plane (processing thread).
//!
//! # Single-threaded data plane (intentional, G4)
//!
//! C++ `NDPluginDriver` runs `numThreads` worker threads sharing one input
//! queue (`createCallbackThreads`). The Rust port deliberately runs **exactly
//! one** per-plugin data thread driving a `tokio::select!` loop. This is an
//! intentional design choice: a single owner of the processing state removes
//! the C++ worker-pool races (shared `prevUniqueId_`, sort-buffer contention)
//! and keeps array ordering trivially correct. The `NUM_THREADS` / `MAX_THREADS`
//! PVs are therefore not backed by a real worker pool — instead `NumThreads`
//! is validated and clamped to `[1, MaxThreads]` on write and the clamped
//! value is written back, so the PV is honest about the accepted value rather
//! than silently inert.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime, port_runtime_unavailable};
use asyn_rs::user::AsynUser;
use epics_libcom_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

use asyn_rs::port_handle::PortHandle;

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;
use asyn_rs::param::ParamValue;

use super::channel::{
    NDArrayOutput, NDArrayReceiver, NDArraySender, PublishOutcome, ndarray_channel,
};
use super::params::PluginBaseParams;
use super::wiring::{WiringRegistry, upstream_key};

/// Message sent through the param channel from control plane to data plane.
///
/// The channel is FIFO, which is what gives [`PluginParamMsg::Barrier`] its
/// meaning: when the data thread acknowledges a barrier, every `Change`
/// enqueued before it has been fully applied (enable flips, wiring rewires,
/// processor param updates).
#[derive(Debug)]
enum PluginParamMsg {
    /// A param write to apply.
    Change(usize, i32, ParamChangeValue),
    /// Sync barrier — acknowledged (best-effort send of `()`) at full
    /// quiescence: every `Change` enqueued before it has been applied (FIFO
    /// channel) AND the array queue has drained. The second condition exists
    /// because arrays travel a separate channel: without it, a param applied
    /// while an older array still waits in the queue would retroactively
    /// change how that array is processed.
    Barrier(std::sync::mpsc::SyncSender<()>),
}

/// Value sent through the param change channel from control plane to data plane.
#[derive(Debug, Clone)]
pub enum ParamChangeValue {
    Int32(i32),
    Float64(f64),
    Octet(String),
}

impl ParamChangeValue {
    pub fn as_i32(&self) -> i32 {
        match self {
            ParamChangeValue::Int32(v) => *v,
            ParamChangeValue::Float64(v) => *v as i32,
            ParamChangeValue::Octet(_) => 0,
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            ParamChangeValue::Int32(v) => *v as f64,
            ParamChangeValue::Float64(v) => *v,
            ParamChangeValue::Octet(_) => 0.0,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            ParamChangeValue::Octet(s) => Some(s),
            _ => None,
        }
    }
}

/// A single parameter update produced by a plugin's process_array.
pub enum ParamUpdate {
    Int32 {
        reason: usize,
        addr: i32,
        value: i32,
    },
    Float64 {
        reason: usize,
        addr: i32,
        value: f64,
    },
    Octet {
        reason: usize,
        addr: i32,
        value: String,
    },
    Float64Array {
        reason: usize,
        addr: i32,
        value: Vec<f64>,
    },
}

impl ParamUpdate {
    /// Create an Int32 update at addr 0.
    pub fn int32(reason: usize, value: i32) -> Self {
        Self::Int32 {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create a Float64 update at addr 0.
    pub fn float64(reason: usize, value: f64) -> Self {
        Self::Float64 {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create an Int32 update at a specific addr.
    pub fn int32_addr(reason: usize, addr: i32, value: i32) -> Self {
        Self::Int32 {
            reason,
            addr,
            value,
        }
    }
    /// Create a Float64 update at a specific addr.
    pub fn float64_addr(reason: usize, addr: i32, value: f64) -> Self {
        Self::Float64 {
            reason,
            addr,
            value,
        }
    }
    /// Create a Float64Array update at addr 0.
    pub fn float64_array(reason: usize, value: Vec<f64>) -> Self {
        Self::Float64Array {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create a Float64Array update at a specific addr.
    pub fn float64_array_addr(reason: usize, addr: i32, value: Vec<f64>) -> Self {
        Self::Float64Array {
            reason,
            addr,
            value,
        }
    }
    /// Create an Octet (string) update at addr 0.
    pub fn octet(reason: usize, value: String) -> Self {
        Self::Octet {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create an Octet (string) update at a specific addr.
    pub fn octet_addr(reason: usize, addr: i32, value: String) -> Self {
        Self::Octet {
            reason,
            addr,
            value,
        }
    }
}

/// Result of processing one array: output arrays + param updates to write back.
pub struct ProcessResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
    /// When `true`, the output arrays are *scattered* — delivered to a single
    /// downstream consumer in round-robin order rather than broadcast to all.
    /// The target consumer (and reroute-past-full / drop-on-last decisions) is
    /// owned by the runtime delivery path, which holds the persistent cursor
    /// (C++ `NDPluginScatter::nextClient_`); the processor only marks the frame
    /// as a scatter frame.
    pub scatter: bool,
}

impl ProcessResult {
    /// Convenience: sink plugin with only param updates, no output arrays.
    pub fn sink(param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![],
            param_updates,
            scatter: false,
        }
    }

    /// Convenience: passthrough/transform plugin with output arrays but no param updates.
    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            scatter: false,
        }
    }

    /// Convenience: no outputs, no param updates.
    pub fn empty() -> Self {
        Self {
            output_arrays: vec![],
            param_updates: vec![],
            scatter: false,
        }
    }

    /// Convenience: scatter output — deliver to the next downstream consumer in
    /// round-robin order (the runtime owns the cursor and reroute logic).
    pub fn scatter(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            scatter: true,
        }
    }
}

/// Result of handling a control-plane param change.
pub struct ParamChangeResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
}

impl ParamChangeResult {
    pub fn updates(param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![],
            param_updates,
        }
    }

    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
        }
    }

    pub fn combined(output_arrays: Vec<Arc<NDArray>>, param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays,
            param_updates,
        }
    }

    pub fn empty() -> Self {
        Self {
            output_arrays: vec![],
            param_updates: vec![],
        }
    }
}

/// Pure processing logic. No threading concerns.
pub trait NDPluginProcess: Send + 'static {
    /// Process one array. Return output arrays and param updates.
    fn process_array(&mut self, array: &NDArray, pool: &NDArrayPool) -> ProcessResult;

    /// Plugin type name for PLUGIN_TYPE param.
    fn plugin_type(&self) -> &str;

    /// Whether this plugin can process compressed (`codec != None`) arrays
    /// (C++ `compressionAware_`, G3). Defaults to `false`: a plugin that
    /// operates on raw pixels must not be handed compressed bytes — the
    /// runtime drops compressed input and counts it into DroppedArrays.
    /// A codec/file plugin that understands compressed data overrides this.
    fn compression_aware(&self) -> bool {
        false
    }

    /// Whether this plugin delivers arrays to downstream plugins, i.e. the
    /// initial `NDArrayCallbacks` param value. Defaults to `true`: most plugins
    /// do array callbacks. Terminal plugins that never deliver downstream
    /// (`NDPluginStdArrays`, `NDPluginAttribute`, every `NDPluginFile` writer)
    /// override this to `false` so the param reflects the behaviour, matching C
    /// (e.g. `NDPluginFile.cpp:948` `setIntegerParam(NDArrayCallbacks, 0)`).
    fn does_array_callbacks(&self) -> bool {
        true
    }

    /// Register plugin-specific params on the base. Called once during construction.
    fn register_params(
        &mut self,
        _base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        Ok(())
    }

    /// Called when a param changes. Reason is the param index.
    /// Return param updates to be written back to the port driver.
    fn on_param_change(
        &mut self,
        _reason: usize,
        _params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        ParamChangeResult::empty()
    }

    /// Return a handle to the latest NDArray data for array reads.
    /// Override this in plugins like NDPluginStdArrays that serve pixel data
    /// via readInt8Array/readInt16Array/etc.
    fn array_data_handle(&self) -> Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>> {
        None
    }
}

/// Read-only snapshot of param values available to the processing thread.
pub struct PluginParamSnapshot {
    pub enable_callbacks: bool,
    /// The param reason that changed.
    pub reason: usize,
    /// The address (sub-device) that changed.
    pub addr: i32,
    /// The new value.
    pub value: ParamChangeValue,
}

/// One buffered entry in the sort buffer: the output arrays for a uniqueId
/// plus the instant they were inserted (for the per-element staleness
/// deadline — C++ `sortedListElement::insertionTime_`).
struct SortEntry {
    arrays: Vec<Arc<NDArray>>,
    inserted: std::time::Instant,
}

/// Sort buffer for reordering out-of-order output arrays by uniqueId.
///
/// Port of C++ `sortedNDArrayList_` semantics (NDPluginDriver.cpp).
/// Only arrays that arrive *out of order* are buffered here — in-order
/// arrays are emitted immediately by the caller (B2). The drain logic
/// (`drain_ready`) releases the head while the next-expected uniqueId is
/// contiguous OR the head has been buffered longer than `sort_time` (B3).
struct SortBuffer {
    /// Buffered out-of-order arrays keyed by uniqueId.
    entries: BTreeMap<i32, SortEntry>,
    /// uniqueId of the last array emitted downstream (C++ `prevUniqueId_`).
    prev_unique_id: i32,
    /// Whether any array has been emitted yet (C++ `firstOutputArray_`).
    first_output: bool,
    /// Cumulative count of arrays emitted out of order (C++ DisorderedArrays).
    disordered_arrays: i32,
    /// Cumulative count of arrays dropped because the buffer was full
    /// (C++ DroppedOutputArrays — sort-buffer-overflow portion).
    dropped_output_arrays: i32,
}

impl SortBuffer {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            prev_unique_id: 0,
            first_output: true,
            disordered_arrays: 0,
            dropped_output_arrays: 0,
        }
    }

    /// True if `unique_id` follows `prev_unique_id` in order (C++ `orderOK`).
    fn order_ok(&self, unique_id: i32) -> bool {
        unique_id == self.prev_unique_id || unique_id == self.prev_unique_id + 1
    }

    /// Record that an array with `unique_id` was emitted downstream.
    /// Updates `prev_unique_id` and counts a disorder if it was out of order.
    fn note_emitted(&mut self, unique_id: i32) {
        if !self.first_output && !self.order_ok(unique_id) {
            self.disordered_arrays += 1;
        }
        self.first_output = false;
        self.prev_unique_id = unique_id;
    }

    /// Insert an out-of-order array into the sort buffer.
    ///
    /// Returns `false` if the buffer was full and the array was dropped
    /// (C++ NDPluginDriver.cpp:307-316), `true` if buffered.
    fn insert(&mut self, unique_id: i32, arrays: Vec<Arc<NDArray>>, sort_size: i32) -> bool {
        if sort_size > 0 && self.entries.len() as i32 >= sort_size {
            self.dropped_output_arrays += 1;
            return false;
        }
        self.entries
            .entry(unique_id)
            .or_insert_with(|| SortEntry {
                arrays: Vec::new(),
                inserted: std::time::Instant::now(),
            })
            .arrays
            .extend(arrays);
        true
    }

    /// Drain the buffer head-first while either the next expected uniqueId is
    /// contiguous OR the head element has aged past `sort_time` seconds.
    /// Port of C++ `sortingTask` loop (NDPluginDriver.cpp:619-670).
    fn drain_ready(&mut self, sort_time: f64) -> Vec<(i32, Vec<Arc<NDArray>>)> {
        let now = std::time::Instant::now();
        let mut out = Vec::new();
        while let Some((&head_id, entry)) = self.entries.iter().next() {
            let delta = now.duration_since(entry.inserted).as_secs_f64();
            let order_ok = self.order_ok(head_id);
            if (!self.first_output && order_ok) || delta > sort_time {
                let entry = self.entries.remove(&head_id).unwrap();
                self.note_emitted(head_id);
                out.push((head_id, entry.arrays));
            } else {
                break;
            }
        }
        out
    }

    /// Drain every buffered array in uniqueId order, regardless of contiguity
    /// or age. Used when sort mode is turned off.
    fn drain_all(&mut self) -> Vec<(i32, Vec<Arc<NDArray>>)> {
        let entries = std::mem::take(&mut self.entries);
        let mut out = Vec::with_capacity(entries.len());
        for (id, entry) in entries {
            self.note_emitted(id);
            out.push((id, entry.arrays));
        }
        out
    }

    /// Number of uniqueId entries currently buffered.
    fn len(&self) -> i32 {
        self.entries.len() as i32
    }
}

/// Shared processor state protected by a mutex, accessible from both
/// the data thread (non-blocking mode) and the caller thread (blocking mode).
struct SharedProcessorInner<P: NDPluginProcess> {
    processor: P,
    output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    pool: Arc<NDArrayPool>,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    port_handle: PortHandle,
    /// ArrayCounter — owned in the param library (C++ `NDArrayCounter`), held
    /// here only as a working copy that is kept in sync with the param so a
    /// control-plane write of `ARRAY_COUNTER` resets it (B12).
    array_counter: i32,
    /// Param index for STD_ARRAY_DATA (if this is a StdArrays plugin).
    std_array_data_param: Option<usize>,
    /// NDArrayCallbacks (C++ `NDArrayCallbacks`): when `false`, the plugin
    /// still processes and updates its metadata params but does NOT deliver the
    /// output array downstream — `endProcessCallbacks` (NDPluginDriver.cpp:
    /// 257-265) returns before the sort/throttle/`doCallbacksGenericPointer`
    /// path. Distinct from `enabled` (`EnableCallbacks`), which gates whether
    /// the plugin processes the input at all.
    array_callbacks: bool,
    /// MinCallbackTime throttling: minimum seconds between process calls.
    min_callback_time: f64,
    /// Last time process_and_publish was called (for throttling).
    last_process_time: Option<std::time::Instant>,
    /// Sort mode: 0 = disabled, 1 = sorted output.
    sort_mode: i32,
    /// Sort time: seconds — per-element staleness deadline for the sort buffer.
    sort_time: f64,
    /// Sort size: maximum number of uniqueId entries in the sort buffer.
    sort_size: i32,
    /// Sort buffer for reordering output arrays by uniqueId.
    sort_buffer: SortBuffer,
    /// Cumulative count of dropped *input* arrays (full queue / compression
    /// gate / MinCallbackTime throttle). Shared with every upstream sender so
    /// full-queue drops are visible here (G1, B1, B5).
    dropped_arrays: Arc<std::sync::atomic::AtomicI32>,
    /// Whether this plugin can process compressed (`codec != None`) arrays.
    /// A non-compression-aware plugin drops compressed input (G3).
    compression_aware: bool,
    /// Output byte-rate limit (C++ `MaxByteRate`); 0 disables throttling.
    max_byte_rate: f64,
    /// Token-bucket throttler enforcing `max_byte_rate` on the output path (G7).
    throttler: super::throttler::Throttler,
    /// Last *input* array, cached for ProcessPlugin re-injection
    /// (C++ `pPrevInputArray_`, G5). Released on `EnableCallbacks=0` (B6).
    prev_input_array: Option<Arc<NDArray>>,
    /// Previous array dimensions, for firing an NDDimensions int32-array
    /// callback when dimensions change (C++ `dimsPrev_`, G8).
    dims_prev: Vec<i32>,
    /// Source address selected via the NDArrayAddr PV (C++ `NDArrayAddr`, G6).
    nd_array_addr: i32,
    /// MaxThreads — the clamp ceiling for NumThreads (C++ `MaxThreads`).
    max_threads: i32,
    /// NumThreads — validated/clamped to [1, MaxThreads] on write (G4).
    num_threads: i32,
}

impl<P: NDPluginProcess> SharedProcessorInner<P> {
    fn should_throttle(&self) -> bool {
        if self.min_callback_time <= 0.0 {
            return false;
        }
        if let Some(last) = self.last_process_time {
            last.elapsed().as_secs_f64() < self.min_callback_time
        } else {
            false
        }
    }

    /// Byte cost of an array for throttling (C++ `NDPluginDriver::throttled`):
    /// compressed size when a codec is present, else total raw bytes.
    fn array_byte_cost(array: &NDArray) -> f64 {
        match &array.codec {
            Some(c) => c.compressed_size as f64,
            None => array.info().total_bytes as f64,
        }
    }

    /// Apply the output throttle to one array. Returns `true` if the array
    /// should be emitted, `false` if it was dropped (and counts the drop).
    fn throttle_ok(&mut self, array: &NDArray) -> bool {
        if self.max_byte_rate == 0.0 {
            return true;
        }
        let cost = Self::array_byte_cost(array);
        if self.throttler.try_take(cost) {
            true
        } else {
            self.sort_buffer.dropped_output_arrays += 1;
            false
        }
    }

    /// Route output arrays through the throttle, the in-order fast path, and
    /// the sort buffer. Returns arrays ready to emit *now*, in order.
    ///
    /// Port of C++ `endProcessCallbacks` (NDPluginDriver.cpp:295-328): an
    /// array whose uniqueId is contiguous with `prevUniqueId_` is emitted
    /// immediately (B2); only out-of-order arrays enter the sort buffer.
    /// Disordered arrays are counted at emission time in both modes (B4).
    fn route_output_arrays(&mut self, arrays: Vec<Arc<NDArray>>) -> Vec<Arc<NDArray>> {
        let mut ready = Vec::new();
        for arr in arrays {
            if !self.throttle_ok(&arr) {
                continue; // G7: dropped by MaxByteRate throttle
            }
            let uid = arr.unique_id;
            if self.sort_mode != 0
                && !self.sort_buffer.first_output
                && !self.sort_buffer.order_ok(uid)
            {
                // Out of order with sort mode on: buffer it (B2/B3).
                self.sort_buffer.insert(uid, vec![arr], self.sort_size);
            } else {
                // In order (or sort mode off): emit immediately, count disorder.
                self.sort_buffer.note_emitted(uid);
                ready.push(arr);
            }
        }
        // After emitting in-order arrays, the sort buffer head may now be
        // contiguous — release any newly-ready run (C++ sortingTask).
        if self.sort_mode != 0 {
            for (_id, mut bucket) in self.sort_buffer.drain_ready(self.sort_time) {
                ready.append(&mut bucket);
            }
        }
        ready
    }

    /// Process array and return a `ProcessOutput`. Does NOT send to actor.
    /// Direct interrupts (std_array_data_param) happen here (sync).
    /// The returned output must be published and flushed by the caller in async context.
    fn process_and_publish(&mut self, array: &Arc<NDArray>) -> Option<ProcessOutput> {
        // A MinCallbackTime-throttled array is silently skipped: C++
        // driverCallback (NDPluginDriver.cpp:405-450) falls through the
        // `deltaTime <= minCallbackTime` gate straight to callParamCallbacks()
        // without touching any param — DroppedArrays is incremented ONLY on a
        // compression-unaware array (:388) or a full message queue (:440), not
        // on throttle. Post nothing and do not count the frame.
        if self.should_throttle() {
            return None;
        }
        // R2/G5: cache the input array for ProcessPlugin re-injection only
        // for arrays that actually pass the MinCallbackTime gate and are
        // processed. C++ sets pPrevInputArray_ in beginProcessCallbacks,
        // which runs inside processCallbacks — never for throttled frames.
        self.prev_input_array = Some(Arc::clone(array));
        let t0 = std::time::Instant::now();
        let result = self.processor.process_array(array, &self.pool);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.last_process_time = Some(t0);

        // C++ endProcessCallbacks (NDPluginDriver.cpp:257-265): when
        // NDArrayCallbacks==0 the method caches the array and returns BEFORE the
        // throttle / sort-admission / `doCallbacksGenericPointer` path. So a
        // non-delivering frame must not enter the MaxByteRate throttle or the
        // sort buffer — only the metadata params (beginProcessCallbacks) are
        // published. Route (throttle + sort) only when delivering.
        //
        // NDPluginStdArrays is the exception: it sets NDArrayCallbacks=0 yet
        // still serves its typed-array waveforms (STD_ARRAY_DATA). Those are
        // NOT the `doCallbacksGenericPointer` downstream path — they fire
        // regardless of NDArrayCallbacks and ARE subject to the MaxByteRate
        // throttle (NDPluginStdArrays.cpp:58 per-interface `throttled()`). So
        // route whenever we deliver downstream OR serve the StdArray waveforms.
        let produced = result.output_arrays.len();
        let ready = if self.array_callbacks || self.std_array_data_param.is_some() {
            self.route_output_arrays(result.output_arrays)
        } else {
            Vec::new()
        };
        // A StdArrays frame that produced a waveform which the MaxByteRate
        // throttle then dropped (`produced > 0` but `ready` empty) must not
        // advance ArrayCounter — C nets it back out
        // (NDPluginStdArrays.cpp:202-211).
        let count_frame =
            !(self.std_array_data_param.is_some() && produced > 0 && ready.is_empty());
        let mut output = self.build_publish_batch(
            ready,
            result.param_updates,
            result.scatter,
            Some(array.as_ref()),
            elapsed_ms,
            self.array_callbacks,
            count_frame,
        );
        output.batch.merge(self.build_status_params_batch());
        Some(output)
    }

    /// A param batch carrying only the current DroppedArrays / queue counters,
    /// used when an array is dropped before processing (B5).
    fn dropped_arrays_only_batch(&self) -> ProcessOutput {
        ProcessOutput {
            arrays: vec![],
            scatter: false,
            batch: self.build_status_params_batch(),
        }
    }

    /// Re-inject the cached previous input array through the normal process
    /// path (C++ ProcessPlugin, NDPluginDriver.cpp:739-746, G5).
    fn process_plugin(&mut self) -> Option<ProcessOutput> {
        let prev = self.prev_input_array.clone()?;
        self.process_and_publish(&prev)
    }

    /// Flush the sort buffer head-first while contiguous or stale (C++
    /// sortingTask periodic tick). Does NOT drain non-contiguous fresh arrays.
    fn tick_sort_buffer(&mut self) -> ProcessOutput {
        let entries = self.sort_buffer.drain_ready(self.sort_time);
        self.emit_drained(entries)
    }

    /// Drain the entire sort buffer in uniqueId order (sort mode turned off).
    fn flush_sort_buffer(&mut self) -> ProcessOutput {
        let entries = self.sort_buffer.drain_all();
        self.emit_drained(entries)
    }

    fn emit_drained(&mut self, entries: Vec<(i32, Vec<Arc<NDArray>>)>) -> ProcessOutput {
        let mut all_arrays = Vec::new();
        let mut combined = ParamBatch::empty();
        for (_unique_id, arrays) in entries {
            // Sort-buffer entries were admitted only while NDArrayCallbacks was
            // on (route_output_arrays runs past the delivery gate); the C++
            // sort thread delivers them regardless of the *current* flag, so
            // they always deliver here.
            let output = self.build_publish_batch(arrays, vec![], false, None, 0.0, true, true);
            all_arrays.extend(output.arrays);
            combined.merge(output.batch);
        }
        combined.merge(self.build_sort_params_batch());
        ProcessOutput {
            arrays: all_arrays,
            scatter: false,
            batch: combined,
        }
    }

    fn build_sort_params_batch(&self) -> ParamBatch {
        use asyn_rs::request::ParamSetValue;
        let sort_free = self.sort_size - self.sort_buffer.len();
        ParamBatch {
            addr0: vec![
                ParamSetValue::new(
                    self.plugin_params.sort_free,
                    0,
                    ParamValue::Int32(sort_free),
                ),
                ParamSetValue::new(
                    self.plugin_params.disordered_arrays,
                    0,
                    ParamValue::Int32(self.sort_buffer.disordered_arrays),
                ),
                ParamSetValue::new(
                    self.plugin_params.dropped_output_arrays,
                    0,
                    ParamValue::Int32(self.sort_buffer.dropped_output_arrays),
                ),
            ],
            extra: std::collections::HashMap::new(),
        }
    }

    /// Build a param batch carrying the runtime status counters:
    /// DroppedArrays (G1) plus the sort/disorder counters.
    fn build_status_params_batch(&self) -> ParamBatch {
        use asyn_rs::request::ParamSetValue;
        let mut batch = self.build_sort_params_batch();
        batch.addr0.push(ParamSetValue::new(
            self.plugin_params.dropped_arrays,
            0,
            ParamValue::Int32(
                self.dropped_arrays
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
        ));
        batch
    }

    /// Build a ProcessOutput: fires direct interrupts (sync) and collects
    /// param updates into a batch. Does NOT publish arrays — the caller
    /// must publish them in async context.
    ///
    /// `deliver` is the NDArrayCallbacks gate (C++ `endProcessCallbacks`,
    /// NDPluginDriver.cpp:257-265): when `false`, the downstream array
    /// delivery — the STD_ARRAY_DATA generic-pointer interrupt and the returned
    /// `ProcessOutput.arrays` — is suppressed, while the metadata params from
    /// `beginProcessCallbacks` (counter, dims, datatype, …) are still set.
    fn build_publish_batch(
        &mut self,
        output_arrays: Vec<Arc<NDArray>>,
        param_updates: Vec<ParamUpdate>,
        scatter: bool,
        fallback_array: Option<&NDArray>,
        elapsed_ms: f64,
        deliver: bool,
        count_frame: bool,
    ) -> ProcessOutput {
        use asyn_rs::request::ParamSetValue;

        let mut addr0: Vec<ParamSetValue> = Vec::new();
        let mut extra: std::collections::HashMap<i32, Vec<ParamSetValue>> =
            std::collections::HashMap::new();

        if let Some(report_arr) = output_arrays.first().map(|a| a.as_ref()).or(fallback_array) {
            // A StdArrays frame whose waveform output the MaxByteRate throttle
            // dropped (`count_frame == false`) must not bump ArrayCounter — C
            // decrements it back so clients monitoring ArrayCounter see no new
            // data (NDPluginStdArrays.cpp:202-211).
            if count_frame {
                self.array_counter += 1;
            }

            // Fire the StdArray waveform interrupt directly (C EPICS pattern).
            // This is NDPluginStdArrays' typed-array callback
            // (NDPluginStdArrays.cpp:71-73 `arrayInterruptCallback`), NOT the
            // `doCallbacksGenericPointer` downstream path: C fires it whether or
            // not NDArrayCallbacks is set (StdArrays defaults NDArrayCallbacks=0),
            // so it must NOT be gated by `deliver`. It fires only with the
            // routed/served output (`output_arrays.first()`), never the
            // `fallback_array`: C skips the interface callback on throttle
            // (NDPluginStdArrays.cpp:58), so a throttled frame leaves
            // `output_arrays` empty and serves nothing.
            if let (Some(param), Some(served)) = (
                self.std_array_data_param,
                output_arrays.first().map(|a| a.as_ref()),
            ) {
                use crate::ndarray::NDDataBuffer;
                use asyn_rs::param::ParamValue;
                let value = match &served.data {
                    NDDataBuffer::I8(v) => {
                        Some(ParamValue::Int8Array(std::sync::Arc::from(v.as_slice())))
                    }
                    NDDataBuffer::U8(v) => Some(ParamValue::Int8Array(std::sync::Arc::from(
                        v.iter().map(|&x| x as i8).collect::<Vec<_>>().as_slice(),
                    ))),
                    NDDataBuffer::I16(v) => {
                        Some(ParamValue::Int16Array(std::sync::Arc::from(v.as_slice())))
                    }
                    NDDataBuffer::U16(v) => Some(ParamValue::Int16Array(std::sync::Arc::from(
                        v.iter().map(|&x| x as i16).collect::<Vec<_>>().as_slice(),
                    ))),
                    NDDataBuffer::I32(v) => {
                        Some(ParamValue::Int32Array(std::sync::Arc::from(v.as_slice())))
                    }
                    NDDataBuffer::U32(v) => Some(ParamValue::Int32Array(std::sync::Arc::from(
                        v.iter().map(|&x| x as i32).collect::<Vec<_>>().as_slice(),
                    ))),
                    NDDataBuffer::I64(v) => {
                        Some(ParamValue::Int64Array(std::sync::Arc::from(v.as_slice())))
                    }
                    NDDataBuffer::U64(v) => Some(ParamValue::Int64Array(std::sync::Arc::from(
                        v.iter().map(|&x| x as i64).collect::<Vec<_>>().as_slice(),
                    ))),
                    NDDataBuffer::F32(v) => {
                        Some(ParamValue::Float32Array(std::sync::Arc::from(v.as_slice())))
                    }
                    NDDataBuffer::F64(v) => {
                        Some(ParamValue::Float64Array(std::sync::Arc::from(v.as_slice())))
                    }
                };
                if let Some(value) = value {
                    let ts = served.timestamp.to_system_time();
                    self.port_handle
                        .interrupts()
                        .notify(asyn_rs::interrupt::InterruptValue {
                            reason: param,
                            addr: 0,
                            value,
                            timestamp: ts,
                            uint32_changed_mask: 0,
                            ..Default::default()
                        });
                }
            }

            let info = report_arr.info();
            // B11: read ColorMode / BayerPattern from the NDArray attributes
            // (C++ beginProcessCallbacks). `info()` already resolves the
            // ColorMode attribute when present; fall back to it for the param.
            let color_mode = report_arr
                .attributes
                .get("ColorMode")
                .and_then(|a| a.value.as_i64())
                .map(|v| v as i32)
                .unwrap_or(info.color_mode as i32);
            let bayer_pattern = report_arr
                .attributes
                .get("BayerPattern")
                .and_then(|a| a.value.as_i64())
                .map(|v| v as i32)
                .unwrap_or(0);

            // G8: fire an int32-array callback on NDDimensions when the array
            // dimensions change (C++ beginProcessCallbacks dimsPrev_). C++ keeps
            // a fixed `dimsPrev_[ND_ARRAY_MAX_DIMS]` zero-filled beyond `ndims`,
            // compares element-wise over all 10 slots, and posts the full
            // 10-element array (NDPluginDriver.cpp:220-231) — so a caget reads
            // NORD=10 with trailing zeros, not `ndims`.
            let mut cur_dims = vec![0i32; crate::ndarray::ND_ARRAY_MAX_DIMS];
            for (slot, d) in cur_dims.iter_mut().zip(
                report_arr
                    .dims
                    .iter()
                    .take(crate::ndarray::ND_ARRAY_MAX_DIMS),
            ) {
                *slot = d.size as i32;
            }
            if cur_dims != self.dims_prev {
                self.dims_prev = cur_dims.clone();
                self.port_handle
                    .interrupts()
                    .notify(asyn_rs::interrupt::InterruptValue {
                        reason: self.ndarray_params.array_dimensions,
                        addr: 0,
                        value: asyn_rs::param::ParamValue::Int32Array(std::sync::Arc::from(
                            cur_dims.as_slice(),
                        )),
                        timestamp: report_arr.timestamp.to_system_time(),
                        uint32_changed_mask: 0,
                        ..Default::default()
                    });
            }

            addr0.extend([
                ParamSetValue::new(
                    self.ndarray_params.array_counter,
                    0,
                    ParamValue::Int32(self.array_counter),
                ),
                ParamSetValue::new(
                    self.ndarray_params.unique_id,
                    0,
                    ParamValue::Int32(report_arr.unique_id),
                ),
                ParamSetValue::new(
                    self.ndarray_params.n_dimensions,
                    0,
                    ParamValue::Int32(report_arr.dims.len() as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.array_size_x,
                    0,
                    ParamValue::Int32(info.x_size as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.array_size_y,
                    0,
                    ParamValue::Int32(info.y_size as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.array_size_z,
                    0,
                    ParamValue::Int32(info.color_size as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.array_size,
                    0,
                    ParamValue::Int32(info.total_bytes as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.data_type,
                    0,
                    ParamValue::Int32(report_arr.data.data_type() as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.color_mode,
                    0,
                    ParamValue::Int32(color_mode),
                ),
                ParamSetValue::new(
                    self.ndarray_params.bayer_pattern,
                    0,
                    ParamValue::Int32(bayer_pattern),
                ),
                ParamSetValue::new(
                    self.ndarray_params.timestamp_rbv,
                    0,
                    // C `setDoubleParam(NDTimeStamp, pArray->timeStamp)`
                    // (NDPluginDriver.cpp:217) — the standalone double, which a
                    // driver may set from its own clock; NDEpicsTSSec/nSec below
                    // carry epicsTS (`:218-219`).
                    ParamValue::Float64(report_arr.time_stamp),
                ),
                ParamSetValue::new(
                    self.ndarray_params.epics_ts_sec,
                    0,
                    ParamValue::Int32(report_arr.timestamp.sec as i32),
                ),
                ParamSetValue::new(
                    self.ndarray_params.epics_ts_nsec,
                    0,
                    ParamValue::Int32(report_arr.timestamp.nsec as i32),
                ),
            ]);

            // NDCodec / NDCompressedSize — C++ beginProcessCallbacks
            // (NDPluginDriver.cpp:213-214) sets these on every array. An
            // uncompressed array carries an empty codec name and a
            // compressedSize equal to the raw byte count (matching the
            // driver-base path in ndarray_driver::prepare_array).
            match &report_arr.codec {
                Some(codec) => {
                    addr0.push(ParamSetValue::new(
                        self.ndarray_params.codec,
                        0,
                        ParamValue::Octet(codec.name.as_str().to_string()),
                    ));
                    addr0.push(ParamSetValue::new(
                        self.ndarray_params.compressed_size,
                        0,
                        ParamValue::Int32(codec.compressed_size as i32),
                    ));
                }
                None => {
                    addr0.push(ParamSetValue::new(
                        self.ndarray_params.codec,
                        0,
                        ParamValue::Octet(String::new()),
                    ));
                    addr0.push(ParamSetValue::new(
                        self.ndarray_params.compressed_size,
                        0,
                        ParamValue::Int32(info.total_bytes as i32),
                    ));
                }
            }
        }

        addr0.push(ParamSetValue::new(
            self.plugin_params.execution_time,
            0,
            ParamValue::Float64(elapsed_ms),
        ));

        // ArrayRate_RBV is computed by a calc record in the DB template
        // (SCAN "1 second", reading ArrayCounter_RBV delta), not in Rust.

        // Plugin-specific param updates.
        for update in &param_updates {
            match update {
                ParamUpdate::Int32 {
                    reason,
                    addr,
                    value,
                } => {
                    let pv = ParamSetValue::new(*reason, *addr, ParamValue::Int32(*value));
                    if *addr == 0 {
                        addr0.push(pv);
                    } else {
                        extra.entry(*addr).or_default().push(pv);
                    }
                }
                ParamUpdate::Float64 {
                    reason,
                    addr,
                    value,
                } => {
                    let pv = ParamSetValue::new(*reason, *addr, ParamValue::Float64(*value));
                    if *addr == 0 {
                        addr0.push(pv);
                    } else {
                        extra.entry(*addr).or_default().push(pv);
                    }
                }
                ParamUpdate::Octet {
                    reason,
                    addr,
                    value,
                } => {
                    let pv = ParamSetValue::new(*reason, *addr, ParamValue::Octet(value.clone()));
                    if *addr == 0 {
                        addr0.push(pv);
                    } else {
                        extra.entry(*addr).or_default().push(pv);
                    }
                }
                ParamUpdate::Float64Array {
                    reason,
                    addr,
                    value,
                } => {
                    let pv = ParamSetValue::new(
                        *reason,
                        *addr,
                        ParamValue::Float64Array(value.clone().into()),
                    );
                    if *addr == 0 {
                        addr0.push(pv);
                    } else {
                        extra.entry(*addr).or_default().push(pv);
                    }
                }
            }
        }

        ProcessOutput {
            // NDArrayCallbacks==0 suppresses downstream delivery (C++
            // endProcessCallbacks early-return); the metadata params above are
            // still published.
            arrays: if deliver { output_arrays } else { Vec::new() },
            scatter,
            batch: ParamBatch { addr0, extra },
        }
    }
}

/// Output from processing: arrays to publish + param batch to flush.
struct ProcessOutput {
    arrays: Vec<Arc<NDArray>>,
    scatter: bool,
    batch: ParamBatch,
}

impl ProcessOutput {
    /// Publish arrays to downstream senders (async, concurrent fan-out).
    ///
    /// Broadcast frames are published to every sender concurrently (independent
    /// backpressure per sender). Scatter frames are routed to a single consumer
    /// via `scatter_publish`, which advances the persistent `scatter_cursor`.
    /// Arrays are published in order — the next array's fan-out starts only
    /// after the previous one completes.
    async fn publish_arrays(&self, senders: &[NDArraySender], scatter_cursor: &mut usize) {
        for arr in &self.arrays {
            if self.scatter {
                Self::scatter_publish(arr, senders, scatter_cursor).await;
            } else {
                let futs = senders.iter().map(|s| s.publish(arr.clone()));
                futures_util::future::join_all(futs).await;
            }
        }
    }

    /// Deliver one array to the next downstream consumer in round-robin order,
    /// rerouting past full queues — a port of C++
    /// `NDPluginScatter::doNDArrayCallbacks` (NDPluginScatter.cpp:59-90).
    ///
    /// `cursor` is the persistent `nextClient_`: it advances by one per
    /// *attempt*, so a frame that reroutes past a full consumer leaves the
    /// cursor pointing just past the consumer it actually delivered to (not
    /// merely one past the starting point). Walking begins at `cursor % n` and
    /// makes at most `n` attempts. A full (or disabled/closed) consumer is
    /// rerouted past unless this is the last attempt; only the last node is
    /// allowed to drop the array (C++ sets `auxStatus=asynSuccess` for the last
    /// node so its full queue drops rather than reroutes). Earlier full
    /// consumers are passed `is_last=false` so the rerouted-away drop is *not*
    /// counted (C++ `ignoreQueueFull`, NDPluginDriver.cpp:406,433-442).
    ///
    /// Routing is over the *enabled* senders only: a downstream with callbacks
    /// disabled is unregistered from the interrupt list in C
    /// (`setArrayInterrupt(0)`) and is therefore not a scatter target, so it
    /// must not consume a round-robin slot.
    async fn scatter_publish(arr: &Arc<NDArray>, senders: &[NDArraySender], cursor: &mut usize) {
        let active: Vec<&NDArraySender> = senders.iter().filter(|s| s.is_enabled()).collect();
        let n = active.len();
        if n == 0 {
            return;
        }
        for attempt in 0..n {
            let target = *cursor % n;
            *cursor = cursor.wrapping_add(1);
            let is_last = attempt == n - 1;
            match active[target].publish_scatter(arr.clone(), is_last).await {
                // Delivered: done. (In blocking mode publish always delivers,
                // so the loop breaks on the first attempt — matching C++ where
                // a blocking scatter calls processCallbacks inline and breaks.)
                PublishOutcome::Delivered => break,
                // Full / disabled / closed: reroute to the next consumer unless
                // this was the last attempt (then the array is dropped — already
                // counted by publish_scatter when is_last).
                PublishOutcome::DroppedQueueFull
                | PublishOutcome::Disabled
                | PublishOutcome::ChannelClosed => {
                    if is_last {
                        break;
                    }
                }
            }
        }
    }
}

/// Collected param updates ready to be flushed to the actor.
/// Produced by `build_publish_batch()`, consumed by async `flush()`.
struct ParamBatch {
    addr0: Vec<asyn_rs::request::ParamSetValue>,
    extra: std::collections::HashMap<i32, Vec<asyn_rs::request::ParamSetValue>>,
}

impl ParamBatch {
    fn empty() -> Self {
        Self {
            addr0: Vec::new(),
            extra: std::collections::HashMap::new(),
        }
    }

    fn merge(&mut self, other: ParamBatch) {
        self.addr0.extend(other.addr0);
        for (addr, updates) in other.extra {
            self.extra.entry(addr).or_default().extend(updates);
        }
    }

    /// Flush via reliable async enqueue. Call from async context.
    async fn flush(self, port: &asyn_rs::port_handle::PortHandle) {
        if !self.addr0.is_empty() {
            if let Err(e) = port.set_params_and_notify(0, self.addr0).await {
                eprintln!("plugin param flush error (addr 0): {e}");
            }
        }
        for (addr, updates) in self.extra {
            if let Err(e) = port.set_params_and_notify(addr, updates).await {
                eprintln!("plugin param flush error (addr {addr}): {e}");
            }
        }
    }
}

/// The `NDArrayAddr`, `maxThreads` and initial `numThreads` that C takes as
/// `NDPluginDriver` constructor arguments (NDPluginDriver.cpp:153, :158, :159).
///
/// The port's `*Configure` entry points take none of them: a plugin runtime
/// spawns exactly one `plugin-data-<port>` thread and its `WiringRegistry` is
/// keyed by port name, address 0. They are named rather than inlined so the
/// constructor's param block and `SharedProcessorInner`'s initial state cannot
/// drift — publishing one value to the RBV and running on another is what a
/// reader of MaxThreads_RBV would have no way to detect.
const PLUGIN_NDARRAY_ADDR: i32 = 0;
const PLUGIN_MAX_THREADS: i32 = 1;
const PLUGIN_NUM_THREADS: i32 = 1;

/// PortDriver implementation for a plugin's control plane.
#[allow(dead_code)]
pub struct PluginPortDriver {
    base: PortDriverBase,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    param_change_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
    /// Optional handle to the latest NDArray for array read methods (used by StdArrays).
    array_data: Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>>,
    /// Param index for STD_ARRAY_DATA (triggers I/O Intr on ArrayData waveform).
    std_array_data_param: Option<usize>,
}

impl PluginPortDriver {
    fn new<P: NDPluginProcess>(
        port_name: &str,
        plugin_type_name: &str,
        queue_size: usize,
        ndarray_port: &str,
        max_addr: usize,
        param_change_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
        processor: &mut P,
        array_data: Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>>,
        pool: &NDArrayPool,
    ) -> AsynResult<Self> {
        let mut base = PortDriverBase::new(
            port_name,
            max_addr,
            PortFlags {
                can_block: true,
                ..Default::default()
            },
        );

        let ndarray_params = NDArrayDriverParams::create(&mut base)?;
        let plugin_params = PluginBaseParams::create(&mut base)?;

        // C++ `NDPluginDriver::NDPluginDriver` (NDPluginDriver.cpp:152-160)
        // initialises exactly these nine read-only / read-back params here,
        // under a comment that states the mechanism: "If a value is not set
        // here then the read request will return an error (uninitialized)".
        // `PluginPortDriver` does not override `read_int32`, so an unset one
        // reaches `get_int32_strict` (asyn-rs/src/port.rs:1663-1665) and the
        // record it feeds sits UDF/INVALID until something else happens to
        // write it — which for MaxThreads_RBV (SCAN "I/O Intr", no PINI, no
        // output partner) is never.
        base.set_string_param(plugin_params.nd_array_port, 0, ndarray_port.into())?;
        base.set_int32_param(plugin_params.nd_array_addr, 0, PLUGIN_NDARRAY_ADDR)?;
        base.set_int32_param(plugin_params.dropped_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.dropped_output_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.queue_size, 0, queue_size as i32)?;
        // C `:157` — an empty queue has every slot free. The param is spelled
        // QUEUE_FREE (params.rs:40) whatever the struct field is called, so 0
        // here published a permanently full queue on an idle plugin.
        base.set_int32_param(plugin_params.queue_use, 0, queue_size as i32)?;
        base.set_int32_param(plugin_params.max_threads, 0, PLUGIN_MAX_THREADS)?;
        base.set_int32_param(plugin_params.num_threads, 0, PLUGIN_NUM_THREADS)?;
        // C `:160` passes its `blockingCallbacks` constructor argument; the
        // port has no such argument and starts `blocking_mode` false.
        base.set_int32_param(plugin_params.blocking_callbacks, 0, 0)?;

        // EnableCallbacks=0 (Disable) by default, matching EPICS ADCore.
        base.set_int32_param(plugin_params.enable_callbacks, 0, 0)?;
        base.set_string_param(plugin_params.plugin_type, 0, plugin_type_name.into())?;

        // C++ `NDPluginDriver` derives from `asynNDArrayDriver`, so the base
        // constructor's read-only block (asynNDArrayDriver.cpp:954-1005) runs
        // for every plugin as well.
        crate::driver::ndarray_driver::init_read_only_params(
            &mut base,
            &ndarray_params,
            port_name,
        )?;
        crate::driver::ndarray_driver::refresh_pool_stats(&mut base, &ndarray_params, pool)?;
        // Not in C's block: NDArrayCallbacks is a database parameter there,
        // but a plugin that never emits arrays must publish 0 rather than let
        // the DB turn its output on.
        base.set_int32_param(
            ndarray_params.array_callbacks,
            0,
            processor.does_array_callbacks() as i32,
        )?;
        // Not in C's block either — `NDFullFileName_RBV` is only written by a
        // file plugin after a successful write.
        base.set_string_param(ndarray_params.full_file_name, 0, "".into())?;
        // Create STD_ARRAY_DATA param for StdArrays plugins (triggers I/O Intr on ArrayData waveform)
        let std_array_data_param = if array_data.is_some() {
            Some(base.create_param("STD_ARRAY_DATA", asyn_rs::param::ParamType::GenericPointer)?)
        } else {
            None
        };

        // Let the processor register its plugin-specific params
        processor.register_params(&mut base)?;

        Ok(Self {
            base,
            ndarray_params,
            plugin_params,
            param_change_tx,
            array_data,
            std_array_data_param,
        })
    }
}

/// Copy source slice directly into destination buffer, returning elements copied.
fn copy_direct<T: Copy>(src: &[T], dst: &mut [T]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// Convert and copy source slice into destination buffer element-by-element.
fn copy_convert<S, D>(src: &[S], dst: &mut [D]) -> usize
where
    S: CastToF64 + Copy,
    D: CastFromF64 + Copy,
{
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] = D::cast_from_f64(src[i].cast_to_f64());
    }
    n
}

/// Cast an integer source element to an integer destination element with C
/// cast semantics. C++ `NDArrayPool::convert` (`NDArrayPool.cpp:388`,
/// `convertType`: `*pDataOut++ = (dataTypeOut)(*pDataIn++)`; and `:466`,
/// `convertDim`) performs a plain C cast between integer types. A C cast:
///   - same-width sign change is a bitwise reinterpret
///     (`(epicsInt8)(epicsUInt8)255 == -1`);
///   - narrowing truncates to the low bits, wrapping
///     (`(epicsInt8)(epicsUInt16)300 == 44`);
///   - widening sign/zero-extends exactly.
///
/// Rust's `as` between integer types implements exactly these semantics. The
/// f64 round-trip in [`copy_convert`] does NOT: it saturates on narrowing
/// (`300.0 as i8 == 127`), diverging from C++. So every integer-source ->
/// integer-target NDArray array read must go through this C-cast path, not
/// `copy_convert`.
trait CCastTo<D> {
    fn ccast(self) -> D;
}
macro_rules! impl_ccast {
    ( $src:ty => $( $dst:ty ),+ ) => {
        $(
            impl CCastTo<$dst> for $src {
                #[inline]
                fn ccast(self) -> $dst {
                    self as $dst
                }
            }
        )+
    };
}
impl_ccast!(i8 => i16, i32, i64);
impl_ccast!(u8 => i8, i16, i32, i64);
impl_ccast!(i16 => i8, i32, i64);
impl_ccast!(u16 => i8, i16, i32, i64);
impl_ccast!(i32 => i8, i16, i64);
impl_ccast!(u32 => i8, i16, i32, i64);
impl_ccast!(i64 => i8, i16, i32);
impl_ccast!(u64 => i8, i16, i32, i64);

/// Copy an integer source slice into an integer destination buffer using C
/// cast semantics (see [`CCastTo`]) — truncating on narrowing, never
/// saturating.
fn copy_ccast<S, D>(src: &[S], dst: &mut [D]) -> usize
where
    S: CCastTo<D> + Copy,
    D: Copy,
{
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] = src[i].ccast();
    }
    n
}

/// Helper trait for `as f64` casts (handles lossy conversions like i64/u64).
trait CastToF64 {
    fn cast_to_f64(self) -> f64;
}

impl CastToF64 for i8 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u8 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i16 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u16 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i64 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u64 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for f32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for f64 {
    fn cast_to_f64(self) -> f64 {
        self
    }
}

/// Helper trait for `as` casts from f64.
trait CastFromF64 {
    fn cast_from_f64(v: f64) -> Self;
}

impl CastFromF64 for i8 {
    fn cast_from_f64(v: f64) -> Self {
        v as i8
    }
}
impl CastFromF64 for i16 {
    fn cast_from_f64(v: f64) -> Self {
        v as i16
    }
}
impl CastFromF64 for i32 {
    fn cast_from_f64(v: f64) -> Self {
        v as i32
    }
}
impl CastFromF64 for i64 {
    fn cast_from_f64(v: f64) -> Self {
        v as i64
    }
}
impl CastFromF64 for f32 {
    fn cast_from_f64(v: f64) -> Self {
        v as f32
    }
}
impl CastFromF64 for f64 {
    fn cast_from_f64(v: f64) -> Self {
        v
    }
}

/// Copy NDArray data into the output buffer with type conversion.
/// Returns the number of elements copied, or 0 if no data is available.
macro_rules! impl_read_array {
    (
        $self:expr, $buf:expr, $direct_variant:ident,
        ccast: [ $( $ccast_variant:ident ),* ],
        convert: [ $( $variant:ident ),* ]
    ) => {{
        use crate::ndarray::NDDataBuffer;
        let handle = match &$self.array_data {
            Some(h) => h,
            None => return Ok(0),
        };
        let guard = handle.lock();
        let array = match &*guard {
            Some(a) => a,
            None => return Ok(0),
        };
        let n = match &array.data {
            NDDataBuffer::$direct_variant(v) => copy_direct(v, $buf),
            $( NDDataBuffer::$ccast_variant(v) => copy_ccast(v, $buf), )*
            $( NDDataBuffer::$variant(v) => copy_convert(v, $buf), )*
        };
        Ok(n)
    }};
}

impl PortDriver for PluginPortDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn io_write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        let addr = user.addr;
        self.base.set_int32_param(reason, addr, value)?;
        self.base.call_param_callbacks(addr)?;
        // B14: reliable send on an unbounded channel — never drop param changes.
        let _ = self.param_change_tx.send(PluginParamMsg::Change(
            reason,
            addr,
            ParamChangeValue::Int32(value),
        ));
        Ok(())
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        let addr = user.addr;
        self.base.set_float64_param(reason, addr, value)?;
        self.base.call_param_callbacks(addr)?;
        let _ = self.param_change_tx.send(PluginParamMsg::Change(
            reason,
            addr,
            ParamChangeValue::Float64(value),
        ));
        Ok(())
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let reason = user.reason;
        let addr = user.addr;
        let s = String::from_utf8_lossy(data).into_owned();
        self.base.set_string_param(reason, addr, s.clone())?;
        self.base.call_param_callbacks(addr)?;
        let _ = self.param_change_tx.send(PluginParamMsg::Change(
            reason,
            addr,
            ParamChangeValue::Octet(s),
        ));
        Ok(data.len())
    }

    fn read_int8_array(&mut self, _user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize> {
        // Every integer source -> i8 is a C cast (truncating, per C++
        // NDArrayPool.cpp:388); float sources keep the numeric f64 conversion.
        impl_read_array!(
            self, buf, I8,
            ccast: [U8, I16, U16, I32, U32, I64, U64],
            convert: [F32, F64]
        )
    }

    fn read_int16_array(&mut self, _user: &AsynUser, buf: &mut [i16]) -> AsynResult<usize> {
        impl_read_array!(
            self, buf, I16,
            ccast: [I8, U8, U16, I32, U32, I64, U64],
            convert: [F32, F64]
        )
    }

    fn read_int32_array(&mut self, _user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        impl_read_array!(
            self, buf, I32,
            ccast: [I8, U8, I16, U16, U32, I64, U64],
            convert: [F32, F64]
        )
    }

    fn read_int64_array(&mut self, _user: &AsynUser, buf: &mut [i64]) -> AsynResult<usize> {
        impl_read_array!(
            self, buf, I64,
            ccast: [I8, U8, I16, U16, I32, U32, U64],
            convert: [F32, F64]
        )
    }

    fn read_float32_array(&mut self, _user: &AsynUser, buf: &mut [f32]) -> AsynResult<usize> {
        impl_read_array!(
            self, buf, F32,
            ccast: [],
            convert: [I8, U8, I16, U16, I32, U32, I64, U64, F64]
        )
    }

    fn read_float64_array(&mut self, _user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        impl_read_array!(
            self, buf, F64,
            ccast: [],
            convert: [I8, U8, I16, U16, I32, U32, I64, U64, F32]
        )
    }
}

/// Handle to a running plugin runtime. Provides access to sender and port handle.
#[derive(Clone)]
pub struct PluginRuntimeHandle {
    port_runtime: PortRuntimeHandle,
    array_sender: NDArraySender,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    port_name: String,
    param_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
    pub ndarray_params: NDArrayDriverParams,
    pub plugin_params: PluginBaseParams,
}

impl PluginRuntimeHandle {
    pub fn port_runtime(&self) -> &PortRuntimeHandle {
        &self.port_runtime
    }

    pub fn array_sender(&self) -> &NDArraySender {
        &self.array_sender
    }

    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_output
    }

    /// Block until the plugin's data thread has applied every control-plane
    /// param change submitted before this call.
    ///
    /// `write_*_blocking` on the port handle returns once the port actor has
    /// recorded the write and queued it for the data plane; the data thread
    /// applies it (the EnableCallbacks flip, NDArrayPort/NDArrayAddr rewiring,
    /// processor param updates) asynchronously. This is the fence between the
    /// two planes: it enqueues a barrier behind every already-queued change
    /// and waits for the data thread to acknowledge it — the param channel is
    /// FIFO, so the ack implies every earlier change is fully applied.
    ///
    /// The ack additionally waits for the array queue to drain, so it also
    /// implies every array published before this call has been fully handled
    /// (processed or throttled). Under continuous array traffic the ack is
    /// therefore delayed until the queue momentarily empties.
    ///
    /// Returns `false` if the data thread has exited or `timeout` elapsed.
    pub fn wait_params_applied(&self, timeout: std::time::Duration) -> bool {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        if self.param_tx.send(PluginParamMsg::Barrier(ack_tx)).is_err() {
            return false;
        }
        ack_rx.recv_timeout(timeout).is_ok()
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

/// Create a plugin runtime with control plane (PortActor) and data plane (processing thread).
///
/// Returns:
/// - `PluginRuntimeHandle` for wiring and control
/// - `PortRuntimeHandle` for param I/O
/// - `JoinHandle` for the data processing thread
pub fn create_plugin_runtime<P: NDPluginProcess>(
    port_name: &str,
    processor: P,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    create_plugin_runtime_multi_addr(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
        1,
    )
}

/// Create a plugin runtime with multi-addr support.
///
/// `max_addr` specifies the number of addresses (sub-devices) the port supports.
pub fn create_plugin_runtime_multi_addr<P: NDPluginProcess>(
    port_name: &str,
    mut processor: P,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
    max_addr: usize,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    // Param change channel (control plane -> data plane)
    // B14: unbounded so control-plane param changes (e.g. autosave restoring
    // hundreds of PVs at IOC init) are never silently dropped before the
    // data plane sees them.
    let (param_tx, param_rx) = tokio::sync::mpsc::unbounded_channel::<PluginParamMsg>();
    let handle_param_tx = param_tx.clone();

    // Capture plugin type and array data handle before mutable borrow
    let plugin_type_name = processor.plugin_type().to_string();
    let compression_aware = processor.compression_aware();
    let does_array_callbacks = processor.does_array_callbacks();
    let array_data = processor.array_data_handle();

    // Create the port driver for control plane
    let driver = PluginPortDriver::new(
        port_name,
        &plugin_type_name,
        queue_size,
        ndarray_port,
        max_addr,
        param_tx,
        &mut processor,
        array_data,
        &pool,
    )
    .expect("failed to create plugin port driver");

    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;
    let std_array_data_param = driver.std_array_data_param;

    // Create port runtime (actor thread for param I/O).
    //
    // Constructor-shaped, so a failure here is fatal and cannot be anything
    // else: this function hands back the built plugin and has no error channel
    // to its `*Configure` caller. C's equivalent — `asynPortDriver`'s
    // constructor printing and `throw`ing on a failed `registerPort`
    // (asynPortDriver.cpp:4036-4040) — is caught by iocsh
    // (iocsh.cpp:1274-1284) and the script continues by default
    // (iocsh.cpp:1001, :1129), leaving the C IOC serving without the port. We
    // deviate on purpose: see `port_runtime_unavailable`.
    let (port_runtime, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default())
        .unwrap_or_else(|e| port_runtime_unavailable(port_name, &e));

    // Clone port handle for the data thread to write params back
    let port_handle = port_runtime.port_handle().clone();

    // Array channel (data plane)
    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    // Shared mode flags
    let enabled = Arc::new(AtomicBool::new(false));
    let blocking_mode = Arc::new(AtomicBool::new(false));

    // Shared processor (accessible from data thread)
    let array_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    let array_output_for_handle = array_output.clone();
    // B13/G6: register this plugin's output so the WiringRegistry is the
    // single source of truth for runtime rewiring (PluginManager::add_plugin
    // would also register it, but direct callers must not bypass the
    // registry). Registered under every address in 0..max_addr so a
    // downstream plugin can select a non-zero NDArrayAddr.
    wiring.register_output_addrs(port_name, max_addr, array_output.clone());
    // G1/B1: the DroppedArrays counter is owned by this plugin and shared with
    // every upstream sender so full-queue drops on our input queue are counted.
    let dropped_arrays_counter = array_sender.dropped_arrays_counter().clone();
    let shared = Arc::new(parking_lot::Mutex::new(SharedProcessorInner {
        processor,
        output: array_output,
        pool,
        ndarray_params,
        plugin_params,
        port_handle,
        array_counter: 0,
        std_array_data_param,
        // C++ default NDArrayCallbacks = 1 (deliver downstream); terminal
        // plugins (StdArrays/Attribute/File) override `does_array_callbacks` to 0.
        array_callbacks: does_array_callbacks,
        min_callback_time: 0.0,
        last_process_time: None,
        sort_mode: 0,
        sort_time: 0.0,
        sort_size: 10,
        sort_buffer: SortBuffer::new(),
        dropped_arrays: dropped_arrays_counter,
        compression_aware,
        max_byte_rate: 0.0,
        throttler: super::throttler::Throttler::new(0.0),
        prev_input_array: None,
        dims_prev: vec![0i32; crate::ndarray::ND_ARRAY_MAX_DIMS],
        nd_array_addr: PLUGIN_NDARRAY_ADDR,
        max_threads: PLUGIN_MAX_THREADS,
        num_threads: PLUGIN_NUM_THREADS,
    }));

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();

    let mut array_sender = array_sender;
    array_sender.set_mode_flags(enabled, blocking_mode);

    // Capture wiring info for data loop
    let sender_port_name = port_name.to_string();
    let initial_upstream = ndarray_port.to_string();

    // Spawn data processing thread
    let data_jh = MandatoryThread::new(
        format!("plugin-data-{port_name}"),
        // `asynNDArrayDriver.cpp:878` — `if (priority <= 0) priority =
        // epicsThreadPriorityMedium`, and that is what `NDPluginDriver` hands
        // its callback threads (`NDPluginDriver.cpp:1016`).
        ThreadPriority::Medium,
        // `asynNDArrayDriver.cpp:876` — `if (stackSize <= 0) stackSize =
        // epicsThreadGetStackSize(epicsThreadStackMedium)`.
        StackSizeClass::Medium,
    )
    .spawn(move || {
        plugin_data_loop(
            shared,
            array_rx,
            param_rx,
            plugin_params,
            ndarray_params.array_counter,
            data_enabled,
            data_blocking,
            sender_port_name,
            initial_upstream,
            wiring,
        );
    });

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        array_output: array_output_for_handle,
        port_name: port_name.to_string(),
        param_tx: handle_param_tx,
        ndarray_params,
        plugin_params,
    };

    (handle, data_jh)
}

/// Build a param batch reporting the input-queue depth.
///
/// `QUEUE_SIZE` = total capacity, `QUEUE_FREE` = free slots. G2: the param is
/// named `QUEUE_FREE` and the reconciled semantics are *free slots*, matching
/// C++ `NDPluginDriverQueueFree = queueSize - pending()`.
fn queue_status_batch(
    plugin_params: &PluginBaseParams,
    max_capacity: usize,
    free: i32,
) -> ParamBatch {
    use asyn_rs::request::ParamSetValue;
    ParamBatch {
        addr0: vec![
            ParamSetValue::new(
                plugin_params.queue_size,
                0,
                ParamValue::Int32(max_capacity as i32),
            ),
            ParamSetValue::new(plugin_params.queue_use, 0, ParamValue::Int32(free)),
        ],
        extra: std::collections::HashMap::new(),
    }
}

/// Write a validated/clamped int32 value back into the param library so the
/// RBV reflects the accepted value (G4 NumThreads/MaxThreads clamping).
async fn clamp_writeback(port: &PortHandle, reason: usize, value: i32) {
    use asyn_rs::request::ParamSetValue;
    let _ = port
        .set_params_and_notify(
            0,
            vec![ParamSetValue::new(
                reason,
                0,
                asyn_rs::param::ParamValue::Int32(value),
            )],
        )
        .await;
}

fn plugin_data_loop<P: NDPluginProcess>(
    shared: Arc<parking_lot::Mutex<SharedProcessorInner<P>>>,
    mut array_rx: NDArrayReceiver,
    mut param_rx: tokio::sync::mpsc::UnboundedReceiver<PluginParamMsg>,
    plugin_params: PluginBaseParams,
    array_counter_reason: usize,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    sender_port_name: String,
    initial_upstream: String,
    wiring: Arc<WiringRegistry>,
) {
    let enable_callbacks_reason = plugin_params.enable_callbacks;
    let blocking_callbacks_reason = plugin_params.blocking_callbacks;
    let min_callback_time_reason = plugin_params.min_callback_time;
    let sort_mode_reason = plugin_params.sort_mode;
    let sort_time_reason = plugin_params.sort_time;
    let sort_size_reason = plugin_params.sort_size;
    let nd_array_port_reason = plugin_params.nd_array_port;
    let nd_array_addr_reason = plugin_params.nd_array_addr;
    let process_plugin_reason = plugin_params.process_plugin;
    let max_byte_rate_reason = plugin_params.max_byte_rate;
    let num_threads_reason = plugin_params.num_threads;
    let max_threads_reason = plugin_params.max_threads;
    let array_callbacks_reason = shared.lock().ndarray_params.array_callbacks;
    // G6: the upstream connection is keyed by (port, addr). `current_upstream`
    // is the base port name; `current_addr` is the selected NDArrayAddr; the
    // effective WiringRegistry key is computed by `upstream_key`.
    let mut current_upstream = initial_upstream;
    let mut current_addr: i32 = 0;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Sort flush timer — starts disabled (very long interval).
        // Re-created when sort_time changes.
        let mut sort_flush_interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        let mut sort_flush_active = false;
        // Last published QueueFree value — only flush the queue params when it
        // changes, so a steady queue depth does not spam param callbacks.
        let mut last_queue_free: Option<i32> = None;
        // Persistent scatter cursor (C++ NDPluginScatter::nextClient_): advances
        // per delivery *attempt* across frames so the round-robin survives
        // reroutes past full consumers. One per plugin instance, for its
        // lifetime — matching `nextClient_(1)` set once at construction.
        let mut scatter_cursor: usize = 0;
        // Barriers held until the array queue drains. A barrier acks only at
        // full quiescence — params applied AND no queued arrays — because a
        // param applied while an older array still waits in the queue would
        // retroactively change that array's processing (e.g. a
        // MinCallbackTime reset un-throttling it). See `PluginParamMsg`.
        let mut held_barriers: Vec<std::sync::mpsc::SyncSender<()>> = Vec::new();

        loop {
            // Release held barriers once the array queue is empty. This runs
            // after every arm, and the single-task loop guarantees any
            // already-dequeued array has fully finished processing by now.
            if !held_barriers.is_empty() && array_rx.pending() == 0 {
                for ack in held_barriers.drain(..) {
                    let _ = ack.try_send(());
                }
            }
            tokio::select! {
                msg = array_rx.recv_msg() => {
                    match msg {
                        Some(msg) => {
                            // B6: quiesce is synchronous — if callbacks were
                            // disabled (the param arm flips `enabled` before
                            // any further array message is handled), drop the
                            // array here without processing.
                            if !enabled.load(Ordering::Acquire) {
                                continue;
                            }
                            // Process array and collect output (sync, under lock).
                            let (process_output, senders, port) = {
                                let mut guard = shared.lock();
                                // G3: a non-compression-aware plugin must drop
                                // a compressed array (codec set) and count it
                                // (C++ driverCallback NDPluginDriver.cpp:383-394).
                                let compressed = msg.array.codec.is_some();
                                let output = if compressed && !guard.compression_aware {
                                    guard
                                        .dropped_arrays
                                        .fetch_add(1, Ordering::AcqRel);
                                    Some(guard.dropped_arrays_only_batch())
                                } else {
                                    // R2/G5: process_and_publish caches the
                                    // input array for ProcessPlugin only after
                                    // the MinCallbackTime gate passes — a
                                    // throttled frame is never cached.
                                    guard.process_and_publish(&msg.array)
                                };
                                let senders = guard.output.lock().senders_clone();
                                let port = guard.port_handle.clone();
                                (output, senders, port)
                            };
                            // G2: update QueueSize/QueueFree from the channel
                            // depth (C++ NDPluginDriver.cpp:512-513). QueueFree
                            // = max_capacity - pending. Only flush when the
                            // value changed to avoid no-op param callbacks.
                            let max_cap = array_rx.max_capacity();
                            let free = max_cap.saturating_sub(array_rx.pending()) as i32;
                            let queue_batch = if last_queue_free != Some(free) {
                                last_queue_free = Some(free);
                                Some(queue_status_batch(&plugin_params, max_cap, free))
                            } else {
                                None
                            };
                            // msg dropped here → completion signaled (if tracked)
                            // Publish arrays and flush params outside the lock, in async context.
                            if let Some(po) = process_output {
                                po.publish_arrays(&senders, &mut scatter_cursor).await;
                                po.batch.flush(&port).await;
                            }
                            if let Some(qb) = queue_batch {
                                qb.flush(&port).await;
                            }
                        }
                        None => break,
                    }
                }
                param = param_rx.recv() => {
                    match param {
                        // Barrier: every Change enqueued before it has been
                        // applied by the arms below (FIFO channel). Ack is
                        // deferred to the top-of-loop release, which also
                        // requires the array queue to be drained — a gone
                        // waiter is not an error.
                        Some(PluginParamMsg::Barrier(ack)) => {
                            held_barriers.push(ack);
                        }
                        Some(PluginParamMsg::Change(reason, addr, value)) => {
                            if reason == enable_callbacks_reason {
                                let on = value.as_i32() != 0;
                                enabled.store(on, Ordering::Release);
                                // B6: disabling releases the cached input array
                                // (C++ writeInt32 NDPluginDriver.cpp:712-722).
                                if !on {
                                    shared.lock().prev_input_array = None;
                                }
                            }
                            if reason == blocking_callbacks_reason {
                                blocking_mode.store(value.as_i32() != 0, Ordering::Release);
                            }
                            // NDArrayCallbacks gates downstream array delivery
                            // (C++ endProcessCallbacks NDPluginDriver.cpp:
                            // 257-265). Processing still runs; only delivery is
                            // suppressed when 0.
                            if reason == array_callbacks_reason {
                                shared.lock().array_callbacks = value.as_i32() != 0;
                            }
                            // Handle MinCallbackTime param change
                            if reason == min_callback_time_reason {
                                shared.lock().min_callback_time = value.as_f64();
                            }
                            // G7: MaxByteRate change resets the output throttler
                            // (C++ writeFloat64 NDPluginDriver.cpp:788-790).
                            if reason == max_byte_rate_reason {
                                let rate = value.as_f64();
                                let mut guard = shared.lock();
                                guard.max_byte_rate = rate;
                                guard.throttler.reset(rate);
                            }
                            // G4: NumThreads / MaxThreads are validated and
                            // clamped on write. The Rust port is intentionally
                            // single-threaded per plugin (one tokio task) — see
                            // the module note — so NumThreads is clamped to
                            // [1, MaxThreads] and the clamped value written back
                            // rather than spawning a worker pool.
                            if reason == max_threads_reason {
                                // Scope the guard so it is released before await.
                                let (port, clamped, mt) = {
                                    let mut guard = shared.lock();
                                    guard.max_threads = value.as_i32().max(1);
                                    let clamped =
                                        guard.num_threads.clamp(1, guard.max_threads);
                                    guard.num_threads = clamped;
                                    (guard.port_handle.clone(), clamped, guard.max_threads)
                                };
                                clamp_writeback(&port, num_threads_reason, clamped).await;
                                clamp_writeback(&port, max_threads_reason, mt).await;
                            }
                            if reason == num_threads_reason {
                                let (port, clamped) = {
                                    let mut guard = shared.lock();
                                    let clamped =
                                        value.as_i32().clamp(1, guard.max_threads.max(1));
                                    guard.num_threads = clamped;
                                    (guard.port_handle.clone(), clamped)
                                };
                                clamp_writeback(&port, num_threads_reason, clamped).await;
                            }
                            // G6: NDArrayAddr selects a source address of a
                            // multi-address driver — reconnect on change
                            // (C++ writeInt32 NDPluginDriver.cpp:724-728).
                            if reason == nd_array_addr_reason {
                                let new_addr = value.as_i32();
                                if new_addr != current_addr {
                                    let old_key = upstream_key(&current_upstream, current_addr);
                                    let new_key = upstream_key(&current_upstream, new_addr);
                                    shared.lock().nd_array_addr = new_addr;
                                    match wiring.rewire_by_name(
                                        &sender_port_name,
                                        &old_key,
                                        &new_key,
                                    ) {
                                        Ok(()) => current_addr = new_addr,
                                        Err(e) => {
                                            eprintln!("NDArrayAddr reconnect failed: {e}");
                                            shared.lock().nd_array_addr = current_addr;
                                        }
                                    }
                                }
                            }
                            // G5: ProcessPlugin re-injects the cached input
                            // array (C++ writeInt32 NDPluginDriver.cpp:739-746).
                            if reason == process_plugin_reason && value.as_i32() != 0 {
                                let (process_output, senders, port) = {
                                    let mut guard = shared.lock();
                                    let output = guard.process_plugin();
                                    let senders = guard.output.lock().senders_clone();
                                    let port = guard.port_handle.clone();
                                    (output, senders, port)
                                };
                                if let Some(po) = process_output {
                                    po.publish_arrays(&senders, &mut scatter_cursor).await;
                                    po.batch.flush(&port).await;
                                } else {
                                    // C parity: NDPluginDriver::writeInt32
                                    // (NDPluginDriver.cpp:743) logs this at
                                    // ASYN_TRACE_WARNING, which is OFF in the
                                    // default port trace mask. Gate through the
                                    // port's trace facility so iocInit stays
                                    // silent unless WARNING is enabled, instead
                                    // of an unconditional eprintln! that spams
                                    // stderr on every PINI ProcessPlugin trigger.
                                    // The asyn port registry that exposes the
                                    // per-port mask only exists with the `ioc`
                                    // integration; a bare plugin build has no
                                    // trace facility to consult, so it stays
                                    // silent there too.
                                    #[cfg(feature = "ioc")]
                                    if let Some(entry) =
                                        asyn_rs::asyn_record::get_port(&sender_port_name)
                                    {
                                        asyn_rs::asyn_trace!(
                                            entry.trace,
                                            sender_port_name.as_str(),
                                            asyn_rs::trace::TraceMask::WARNING,
                                            "plugin {sender_port_name}: ProcessPlugin \
                                             requested but no input array cached"
                                        );
                                    }
                                }
                            }
                            // B12: a control-plane write of ArrayCounter resets
                            // the working counter (C++ keeps NDArrayCounter in
                            // the param library; beginProcessCallbacks reads it).
                            if reason == array_counter_reason {
                                shared.lock().array_counter = value.as_i32();
                            }
                            // Handle sort param changes
                            if reason == sort_mode_reason {
                                let mode = value.as_i32();
                                // Scope the guard so clippy can verify the lock
                                // is released before any await.
                                let flush_work = {
                                    let mut guard = shared.lock();
                                    guard.sort_mode = mode;
                                    if mode == 0 {
                                        let output = guard.flush_sort_buffer();
                                        let senders = guard.output.lock().senders_clone();
                                        let port = guard.port_handle.clone();
                                        sort_flush_active = false;
                                        Some((output, senders, port))
                                    } else {
                                        sort_flush_active = guard.sort_time > 0.0;
                                        if sort_flush_active {
                                            let dur = epics_libcom_rs::runtime::time::duration_from_secs(
                                                guard.sort_time,
                                            );
                                            sort_flush_interval = tokio::time::interval(dur);
                                        }
                                        None
                                    }
                                };
                                if let Some((output, senders, port)) = flush_work {
                                    output.publish_arrays(&senders, &mut scatter_cursor).await;
                                    output.batch.flush(&port).await;
                                }
                            }
                            if reason == sort_time_reason {
                                let t = value.as_f64();
                                let mut guard = shared.lock();
                                guard.sort_time = t;
                                if guard.sort_mode != 0 && t > 0.0 {
                                    sort_flush_active = true;
                                    let dur = epics_libcom_rs::runtime::time::duration_from_secs(t);
                                    sort_flush_interval = tokio::time::interval(dur);
                                } else {
                                    sort_flush_active = false;
                                }
                                drop(guard);
                            }
                            if reason == sort_size_reason {
                                shared.lock().sort_size = value.as_i32();
                            }
                            // Handle NDArrayPort rewiring — keyed by (port, addr).
                            if reason == nd_array_port_reason {
                                if let Some(new_port) = value.as_string() {
                                    if new_port != current_upstream {
                                        let old_key =
                                            upstream_key(&current_upstream, current_addr);
                                        let new_key = upstream_key(new_port, current_addr);
                                        match wiring.rewire_by_name(
                                            &sender_port_name,
                                            &old_key,
                                            &new_key,
                                        ) {
                                            Ok(()) => current_upstream = new_port.to_string(),
                                            Err(e) => {
                                                eprintln!("NDArrayPort rewire failed: {e}")
                                            }
                                        }
                                    }
                                }
                            }
                            let snapshot = PluginParamSnapshot {
                                enable_callbacks: enabled.load(Ordering::Acquire),
                                reason,
                                addr,
                                value,
                            };
                            let (process_output, senders, port) = {
                                let mut guard = shared.lock();
                                let t0 = std::time::Instant::now();
                                let result = guard.processor.on_param_change(reason, &snapshot);
                                let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                                let output = if !result.output_arrays.is_empty() || !result.param_updates.is_empty() {
                                    let deliver = guard.array_callbacks;
                                    Some(guard.build_publish_batch(result.output_arrays, result.param_updates, false, None, elapsed_ms, deliver, true))
                                } else {
                                    None
                                };
                                let senders = guard.output.lock().senders_clone();
                                (output, senders, guard.port_handle.clone())
                            };
                            if let Some(po) = process_output {
                                po.publish_arrays(&senders, &mut scatter_cursor).await;
                                po.batch.flush(&port).await;
                            }
                        }
                        None => break,
                    }
                }
                _ = sort_flush_interval.tick(), if sort_flush_active => {
                    // B3: drain head-first while contiguous or past the
                    // staleness deadline — NOT the whole buffer.
                    let (output, senders, port) = {
                        let mut guard = shared.lock();
                        let output = guard.tick_sort_buffer();
                        let senders = guard.output.lock().senders_clone();
                        let port = guard.port_handle.clone();
                        (output, senders, port)
                    };
                    output.publish_arrays(&senders, &mut scatter_cursor).await;
                    output.batch.flush(&port).await;
                }
            }
        }
    });
}

/// Connect a downstream plugin's sender to a plugin runtime's output.
///
/// B13: the upstream's `array_output` is the same `Arc` that every
/// `create_plugin_runtime*` entry point registers in the `WiringRegistry`, so
/// adding a sender here mutates the registry-tracked output — the registry
/// remains the single source of truth for `rewire_by_name`.
pub fn wire_downstream(upstream: &PluginRuntimeHandle, downstream_sender: NDArraySender) {
    upstream.array_output().lock().add(downstream_sender);
}

/// Create a plugin runtime with a pre-wired output (for testing and direct wiring).
pub fn create_plugin_runtime_with_output<P: NDPluginProcess>(
    port_name: &str,
    mut processor: P,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    output: NDArrayOutput,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    // B14: unbounded so control-plane param changes (e.g. autosave restoring
    // hundreds of PVs at IOC init) are never silently dropped before the
    // data plane sees them.
    let (param_tx, param_rx) = tokio::sync::mpsc::unbounded_channel::<PluginParamMsg>();
    let handle_param_tx = param_tx.clone();

    let plugin_type_name = processor.plugin_type().to_string();
    let compression_aware = processor.compression_aware();
    let does_array_callbacks = processor.does_array_callbacks();
    let array_data = processor.array_data_handle();
    let driver = PluginPortDriver::new(
        port_name,
        &plugin_type_name,
        queue_size,
        ndarray_port,
        1,
        param_tx,
        &mut processor,
        array_data,
        &pool,
    )
    .expect("failed to create plugin port driver");

    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;
    let std_array_data_param = driver.std_array_data_param;

    // Fatal for the same reason as `create_plugin_runtime_multi_addr` above:
    // a constructor-shaped creator has nowhere to report to, and the only
    // alternative is a handle to a port that does not exist.
    let (port_runtime, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default())
        .unwrap_or_else(|e| port_runtime_unavailable(port_name, &e));

    let port_handle = port_runtime.port_handle().clone();

    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    let enabled = Arc::new(AtomicBool::new(false));
    let blocking_mode = Arc::new(AtomicBool::new(false));

    let array_output = Arc::new(parking_lot::Mutex::new(output));
    let array_output_for_handle = array_output.clone();
    // B13: register this plugin's output so the WiringRegistry is the single
    // source of truth — an output created via this entry point is otherwise
    // invisible to runtime rewiring.
    wiring.register_output(port_name, array_output.clone());
    // G1/B1: DroppedArrays counter shared with upstream senders.
    let dropped_arrays_counter = array_sender.dropped_arrays_counter().clone();
    let shared = Arc::new(parking_lot::Mutex::new(SharedProcessorInner {
        processor,
        output: array_output,
        pool,
        ndarray_params,
        plugin_params,
        port_handle,
        array_counter: 0,
        std_array_data_param,
        // C++ default NDArrayCallbacks = 1 (deliver downstream); terminal
        // plugins (StdArrays/Attribute/File) override `does_array_callbacks` to 0.
        array_callbacks: does_array_callbacks,
        min_callback_time: 0.0,
        last_process_time: None,
        sort_mode: 0,
        sort_time: 0.0,
        sort_size: 10,
        sort_buffer: SortBuffer::new(),
        dropped_arrays: dropped_arrays_counter,
        compression_aware,
        max_byte_rate: 0.0,
        throttler: super::throttler::Throttler::new(0.0),
        prev_input_array: None,
        dims_prev: vec![0i32; crate::ndarray::ND_ARRAY_MAX_DIMS],
        nd_array_addr: PLUGIN_NDARRAY_ADDR,
        max_threads: PLUGIN_MAX_THREADS,
        num_threads: PLUGIN_NUM_THREADS,
    }));

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();

    let mut array_sender = array_sender;
    array_sender.set_mode_flags(enabled, blocking_mode);

    // Capture wiring info for data loop
    let sender_port_name = port_name.to_string();
    let initial_upstream = ndarray_port.to_string();

    let data_jh = MandatoryThread::new(
        format!("plugin-data-{port_name}"),
        // `asynNDArrayDriver.cpp:878` — `if (priority <= 0) priority =
        // epicsThreadPriorityMedium`, and that is what `NDPluginDriver` hands
        // its callback threads (`NDPluginDriver.cpp:1016`).
        ThreadPriority::Medium,
        // `asynNDArrayDriver.cpp:876` — `if (stackSize <= 0) stackSize =
        // epicsThreadGetStackSize(epicsThreadStackMedium)`.
        StackSizeClass::Medium,
    )
    .spawn(move || {
        plugin_data_loop(
            shared,
            array_rx,
            param_rx,
            plugin_params,
            ndarray_params.array_counter,
            data_enabled,
            data_blocking,
            sender_port_name,
            initial_upstream,
            wiring,
        );
    });

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        array_output: array_output_for_handle,
        port_name: port_name.to_string(),
        param_tx: handle_param_tx,
        ndarray_params,
        plugin_params,
    };

    (handle, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataType, NDDimension};
    use crate::plugin::channel::ndarray_channel;

    /// # Invariant
    ///
    /// MUST: every `plugin-data-*` thread be created through
    /// [`MandatoryThread`], so that a thread the plugin cannot process without
    /// takes the process down rather than the caller's thread.
    ///
    /// The reason this is not the `errlog-and-continue` class:
    /// `NDPluginDriver::createCallbackThreads` builds its workers as
    /// `new epicsThread(...)` (`NDPluginDriver.cpp:1016`), whose constructor
    /// calls `epicsThreadCreateOpt` and `throw unableToCreateThread()` on
    /// failure (`epicsThread.cpp:214-220`) — a thrown failure, not a status
    /// code the plugin inspects and carries on from.
    ///
    /// Where C ends up is **not** where we do, and the difference is
    /// deliberate: iocsh catches whatever a command throws
    /// (`iocsh.cpp:1274-1284`, `"C++ error: ..."`) and a startup script's
    /// default `on error` is `Continue` (`iocsh.cpp:1001`, `:1129`), so C runs
    /// the rest of st.cmd with the plugin's port registered and its worker
    /// threads absent — arrays queue to it and are never processed, silently,
    /// for the life of the IOC. `MandatoryThread::spawn` refuses that state.
    /// The `.expect` it replaced reached neither: on a `panic = "unwind"`
    /// target it unwound one thread and left the same zombie plugin behind.
    ///
    /// Contrast the auxiliary AD threads, which genuinely do errlog-and-continue
    /// and have no site here: the sorting thread (`NDPluginDriver.cpp:1105-1114`,
    /// `asynPrint` + `return asynError`), the queued-array counter
    /// (`asynNDArrayDriver.cpp:1013-1021`, `asynPrint` and no error at all) and
    /// the HDF5 flush task (`NDFileHDF5.cpp:2423-2431`, `printf` + `return`).
    ///
    /// Source inspection, because the defect is a call that is *absent*.
    #[test]
    fn plugin_data_threads_are_mandatory() {
        let prod = match include_str!("runtime.rs").find("\n#[cfg(test)]") {
            Some(i) => &include_str!("runtime.rs")[..i],
            None => include_str!("runtime.rs"),
        };
        assert_eq!(
            prod.matches("MandatoryThread::new(").count(),
            2,
            "`create_plugin_runtime_multi_addr` and `create_plugin_runtime_with_output`"
        );
        let bare = concat!("thread", "::Builder::new()");
        let strays: Vec<&str> = prod
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains(bare) || l.contains(concat!("thread", "::spawn(")))
            .collect();
        assert!(
            strays.is_empty(),
            "a plugin data thread created outside `MandatoryThread` resolves its \
             own spawn failure locally: {strays:?}"
        );
    }

    /// Passthrough processor: returns the input array as-is.
    struct PassthroughProcessor;

    impl NDPluginProcess for PassthroughProcessor {
        fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::arrays(vec![Arc::new(array.clone())])
        }
        fn plugin_type(&self) -> &str {
            "Passthrough"
        }
    }

    /// Sink processor: consumes arrays, returns nothing.
    struct SinkProcessor {
        count: usize,
    }

    impl NDPluginProcess for SinkProcessor {
        fn process_array(&mut self, _array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
            self.count += 1;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Sink"
        }
    }

    fn make_test_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    fn test_wiring() -> Arc<WiringRegistry> {
        Arc::new(WiringRegistry::new())
    }

    /// Fence: wait until the data thread has applied every param change
    /// submitted so far. `write_*_blocking` only guarantees the change is
    /// queued for the data plane; asserting on data-plane behaviour without
    /// this fence is a race.
    fn params_applied(handle: &PluginRuntimeHandle) {
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply queued param changes"
        );
    }

    /// Poll `cond` until it holds; panic after 10 s. Waits on the observable
    /// state itself instead of sleeping a guessed duration, so a loaded
    /// machine cannot flake the test.
    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Enable callbacks on a plugin handle (plugins default to disabled) and
    /// fence until the data thread has actually flipped the enable flag.
    fn enable_callbacks(handle: &PluginRuntimeHandle) {
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        params_applied(handle);
    }

    /// Send an array via the sender from a sync test context.
    /// Uses a dedicated thread with a current-thread runtime to avoid
    /// interfering with the plugin's own runtime.
    fn send_array(sender: &NDArraySender, array: Arc<NDArray>) {
        let sender = sender.clone();
        let jh = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(sender.publish(array));
        });
        jh.join().unwrap();
    }

    #[test]
    fn test_passthrough_runtime() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        // Create downstream receiver
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "PASS1",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // Send an array
        send_array(handle.array_sender(), make_test_array(42));

        // Should come out the other side
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 42);
    }

    #[test]
    fn test_sink_runtime() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        let (handle, _data_jh) = create_plugin_runtime(
            "SINK1",
            SinkProcessor { count: 0 },
            pool,
            10,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // Send arrays - they should be consumed silently
        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        // ArrayCounter advances once per processed frame — both consumed.
        let port = handle.port_runtime().port_handle().clone();
        let counter = handle.ndarray_params.array_counter;
        wait_until("sink to process both arrays", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 2)
        });
        assert_eq!(handle.port_name(), "SINK1");
    }

    #[test]
    fn test_plugin_type_param() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        let (handle, _data_jh) = create_plugin_runtime(
            "TYPE_TEST",
            PassthroughProcessor,
            pool,
            10,
            "",
            test_wiring(),
        );

        // Verify port name
        assert_eq!(handle.port_name(), "TYPE_TEST");
        assert_eq!(handle.port_runtime().port_name(), "TYPE_TEST");
    }

    #[test]
    fn test_ndtimestamp_param_is_the_standalone_double() {
        // R8-66 family: C `setDoubleParam(NDTimeStamp, pArray->timeStamp)`
        // (NDPluginDriver.cpp:217) publishes the array's standalone double —
        // which a driver with a hardware clock sets independently of epicsTS —
        // while NDEpicsTSSec/nSec carry epicsTS (`:218-219`). The plugin runtime
        // published `timestamp.as_f64()` for all three, so NDTimeStamp_RBV
        // reported the epicsTS-derived value.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (ds, _rx) = ndarray_channel("DS_TS", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds);
        let (handle, _jh) = create_plugin_runtime_with_output(
            "TS_PARAM",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle().clone();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.timestamp = crate::timestamp::EpicsTimestamp {
            sec: 1234,
            nsec: 5678,
        };
        arr.time_stamp = 100.5; // hardware clock, unrelated to epicsTS
        send_array(handle.array_sender(), Arc::new(arr));
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert_eq!(
            port.read_float64_blocking(handle.ndarray_params.timestamp_rbv, 0)
                .unwrap(),
            100.5,
            "NDTimeStamp publishes pArray->timeStamp"
        );
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.epics_ts_sec, 0)
                .unwrap(),
            1234
        );
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.epics_ts_nsec, 0)
                .unwrap(),
            5678
        );
    }

    #[test]
    fn test_shutdown_on_handle_drop() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        let (handle, data_jh) = create_plugin_runtime(
            "SHUTDOWN_TEST",
            PassthroughProcessor,
            pool,
            10,
            "",
            test_wiring(),
        );

        // Drop the handle (closes sender channel, which should cause data thread to exit)
        let sender = handle.array_sender().clone();
        drop(handle);
        drop(sender);

        // Data thread should terminate
        let result = data_jh.join();
        assert!(result.is_ok());
    }

    #[test]
    fn test_wire_to_nonzero_ndarray_addr() {
        // G6: a multi-address upstream plugin registers its output under every
        // address in 0..max_addr. A downstream consumer must be able to select
        // NDArrayAddr=1 and actually receive arrays — previously the output was
        // registered under the bare port name only, so the "PORT:1" key was
        // missing and rewire failed with "not found".
        use crate::plugin::wiring::upstream_key;
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = test_wiring();

        // Upstream plugin advertises 2 addresses.
        let (up_handle, _up_jh) = create_plugin_runtime_multi_addr(
            "UP_MULTI",
            PassthroughProcessor,
            pool,
            10,
            "",
            wiring.clone(),
            2,
        );
        enable_callbacks(&up_handle);

        // The "PORT:1" key must resolve to the same output as the bare port.
        let addr0 = wiring.lookup_output("UP_MULTI");
        let addr1 = wiring.lookup_output(&upstream_key("UP_MULTI", 1));
        assert!(addr0.is_some(), "addr 0 output must be registered");
        assert!(
            addr1.is_some(),
            "addr 1 output must be registered for a max_addr=2 port"
        );

        // Wire a downstream consumer to UP_MULTI address 1.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWN_ADDR1", 10);
        wiring
            .rewire(&downstream_sender, "", &upstream_key("UP_MULTI", 1))
            .expect("wiring a consumer to NDArrayAddr=1 must succeed");

        // An array sent through the upstream must reach the addr-1 consumer.
        send_array(up_handle.array_sender(), make_test_array(99));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            received.unique_id, 99,
            "consumer wired to NDArrayAddr=1 must receive upstream arrays"
        );
    }

    #[test]
    fn test_nonblocking_passthrough() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "NB_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(42));

        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 42);
    }

    #[test]
    fn test_blocking_to_nonblocking_switch() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "SWITCH_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // Start in blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Switch back to non-blocking
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        // Send in non-blocking mode — goes through channel to data thread
        send_array(handle.array_sender(), make_test_array(2));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 2);
    }

    #[test]
    fn test_enable_callbacks_disables_processing() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "ENABLE_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );

        // Disable callbacks
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        // Send array — should be silently dropped by sender (callbacks disabled)
        send_array(handle.array_sender(), make_test_array(99));

        // Verify nothing received (with timeout)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(100), downstream_rx.recv()).await
        });
        assert!(
            result.is_err(),
            "should not receive array when callbacks disabled"
        );
    }

    #[test]
    fn test_downstream_receives_multiple() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        let (ds1, mut rx1) = ndarray_channel("DS1", 10);
        let (ds2, mut rx2) = ndarray_channel("DS2", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds1);
        output.add(ds2);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "DS_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(77));

        // Both downstream receivers should have the array
        let r1 = rx1.blocking_recv().unwrap();
        let r2 = rx2.blocking_recv().unwrap();
        assert_eq!(r1.unique_id, 77);
        assert_eq!(r2.unique_id, 77);
    }

    #[test]
    fn test_param_updates_after_send() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        struct ParamTracker;
        impl NDPluginProcess for ParamTracker {
            fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::arrays(vec![Arc::new(array.clone())])
            }
            fn plugin_type(&self) -> &str {
                "ParamTracker"
            }
        }

        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "PARAM_TEST",
            ParamTracker,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // Send array
        send_array(handle.array_sender(), make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Write enable_callbacks — should not crash
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);

        // Still works after param update
        send_array(handle.array_sender(), make_test_array(2));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 2);
    }

    #[test]
    fn test_sort_buffer_reorders_by_unique_id() {
        let mut buf = SortBuffer::new();

        // Insert out of order: 3, 1, 2
        buf.insert(3, vec![make_test_array(3)], 10);
        buf.insert(1, vec![make_test_array(1)], 10);
        buf.insert(2, vec![make_test_array(2)], 10);

        assert_eq!(buf.len(), 3);

        let drained = buf.drain_all();
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3], "should drain in sorted uniqueId order");
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.prev_unique_id, 3);
    }

    #[test]
    fn test_sort_buffer_drain_ready_contiguous() {
        // B3: drain_ready releases the head while the next-expected uniqueId
        // is contiguous, even when later ids are still missing.
        let mut buf = SortBuffer::new();
        // Mark a prior emission (prev=0) so the contiguity path is active;
        // C++ only uses the deadline for the very first output array.
        buf.note_emitted(0);
        buf.insert(1, vec![make_test_array(1)], 10);
        buf.insert(2, vec![make_test_array(2)], 10);
        buf.insert(5, vec![make_test_array(5)], 10); // gap: 3,4 missing

        // sort_time large → only contiguity drives release.
        let drained = buf.drain_ready(100.0);
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2], "contiguous run released; id=5 held by gap");
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn test_sort_buffer_drain_ready_deadline() {
        // B3: a stale head is released past sort_time even with a gap.
        let mut buf = SortBuffer::new();
        buf.note_emitted(1); // prev=1
        buf.insert(5, vec![make_test_array(5)], 10); // out of order
        std::thread::sleep(std::time::Duration::from_millis(30));
        // sort_time=0.01s → head aged past deadline → released.
        let drained = buf.drain_ready(0.01);
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![5], "stale head released via deadline");
    }

    #[test]
    fn test_sort_buffer_detects_disordered_on_emit() {
        // B4: disorder is counted at emission time.
        let mut buf = SortBuffer::new();
        buf.note_emitted(5); // prev=5, first_output now false
        buf.note_emitted(3); // 3 != 5 and != 6 → disordered
        assert_eq!(buf.disordered_arrays, 1);
        buf.note_emitted(4); // 4 != 3 and != 4? 4 == prev+1 → ordered
        assert_eq!(buf.disordered_arrays, 1);
    }

    #[test]
    fn test_sort_buffer_drops_when_full() {
        let mut buf = SortBuffer::new();

        // sort_size=2: third insert is refused.
        assert!(buf.insert(1, vec![make_test_array(1)], 2));
        assert!(buf.insert(2, vec![make_test_array(2)], 2));
        assert!(!buf.insert(3, vec![make_test_array(3)], 2));

        assert_eq!(buf.len(), 2);
        assert_eq!(buf.dropped_output_arrays, 1);
    }

    #[test]
    fn test_constructor_initialises_c_read_only_params() {
        // C++ NDPluginDriver.cpp:152-160 initialises these in the constructor
        // precisely so a read before any array flows returns a value instead
        // of "uninitialized". Nothing here enables callbacks or sends an
        // array: this is the idle plugin an operator sees right after
        // iocInit, which is when QueueFree_RBV read 0 (full) and
        // MaxThreads_RBV read UDF/INVALID.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "CTOR_TEST",
            PassthroughProcessor,
            pool,
            20,
            NDArrayOutput::new(),
            "",
            test_wiring(),
        );
        let port = handle.port_runtime().port_handle();
        let read = |reason: usize| port.read_int32_blocking(reason, 0);

        // C `:156` / `:157` — an idle queue is entirely free.
        assert_eq!(read(handle.plugin_params.queue_size).unwrap(), 20);
        assert_eq!(read(handle.plugin_params.queue_use).unwrap(), 20);
        // C `:153` / `:158` / `:159`. These have no other writer: MaxThreads
        // is a longin with SCAN "I/O Intr", no PINI and no output partner, so
        // an unset param leaves it UDF forever.
        assert_eq!(
            read(handle.plugin_params.nd_array_addr).unwrap(),
            PLUGIN_NDARRAY_ADDR
        );
        assert_eq!(
            read(handle.plugin_params.max_threads).unwrap(),
            PLUGIN_MAX_THREADS
        );
        assert_eq!(
            read(handle.plugin_params.num_threads).unwrap(),
            PLUGIN_NUM_THREADS
        );
        // C `:154` / `:155`.
        assert_eq!(read(handle.plugin_params.dropped_arrays).unwrap(), 0);
        assert_eq!(read(handle.plugin_params.dropped_output_arrays).unwrap(), 0);
    }

    #[test]
    fn test_constructor_initialises_the_ndarray_read_only_block() {
        // C++ `NDPluginDriver` derives from `asynNDArrayDriver`, so the base
        // constructor's block (asynNDArrayDriver.cpp:954-1005) runs for a
        // plugin as well. Read through the port handle, which is the same
        // path a record takes: an unwritten param comes back as an error.
        let pool = Arc::new(NDArrayPool::new(2_097_152));
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "NDCTOR_TEST",
            PassthroughProcessor,
            pool,
            20,
            NDArrayOutput::new(),
            "",
            test_wiring(),
        );
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        for (name, reason, want) in [
            ("ARRAY_SIZE_X", p.array_size_x, 0),
            ("ARRAY_SIZE_Y", p.array_size_y, 0),
            ("ARRAY_SIZE_Z", p.array_size_z, 0),
            ("ARRAY_SIZE", p.array_size, 0),
            ("ND_DIMENSIONS", p.n_dimensions, 0),
            (
                "COLOR_MODE",
                p.color_mode,
                crate::color::NDColorMode::Mono as i32,
            ),
            ("UNIQUE_ID", p.unique_id, 0),
            ("EPICS_TS_SEC", p.epics_ts_sec, 0),
            ("EPICS_TS_NSEC", p.epics_ts_nsec, 0),
            ("BAYER_PATTERN", p.bayer_pattern, 0),
            ("ARRAY_COUNTER", p.array_counter, 0),
            ("NUM_CAPTURED", p.num_captured, 0),
            ("FREE_CAPTURE", p.free_capture, 0),
            (
                "ND_ATTRIBUTES_STATUS",
                p.attributes_status,
                crate::driver::ndarray_driver::ATTR_STATUS_FILE_NOT_FOUND,
            ),
            ("NUM_QUEUED_ARRAYS", p.num_queued_arrays, 0),
            ("POOL_ALLOC_BUFFERS", p.pool_alloc_buffers, 0),
            ("POOL_FREE_BUFFERS", p.pool_free_buffers, 0),
        ] {
            assert_eq!(
                port.read_int32_blocking(reason, 0)
                    .unwrap_or_else(|e| panic!("{name} unset after construction: {e:?}")),
                want,
                "{name}"
            );
        }
        assert_eq!(
            port.read_float64_blocking(p.pool_max_memory, 0)
                .expect("POOL_MAX_MEMORY unset after construction"),
            2.0
        );
        assert_eq!(
            port.read_float64_blocking(p.pool_used_memory, 0)
                .expect("POOL_USED_MEMORY unset after construction"),
            0.0
        );
        assert_eq!(
            port.read_float64_blocking(p.timestamp_rbv, 0)
                .expect("TIME_STAMP unset after construction"),
            0.0
        );
    }

    #[test]
    fn test_sort_mode_runtime_integration() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "SORT_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // Enable sort mode with sort_size=10 and a sort_time deadline.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_size, 0, 10)
            .unwrap();
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.sort_time, 0, 0.1)
            .unwrap();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_mode, 0, 1)
            .unwrap();
        params_applied(&handle);

        // B2: in-order arrays (1,2,3) must be emitted IMMEDIATELY via the
        // fast path — they are NOT delayed by the sort buffer.
        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));
        send_array(handle.array_sender(), make_test_array(3));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fast = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            fast.is_ok(),
            "in-order arrays must be emitted immediately, not buffered"
        );
        assert_eq!(fast.unwrap().unwrap().unique_id, 1);
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 2);
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 3);

        // B3: now send out of order (5 before 4). prev=3, so 4 is in-order
        // and emitted immediately; 5 arrives first, is buffered, then 4
        // unblocks it.
        send_array(handle.array_sender(), make_test_array(5));
        send_array(handle.array_sender(), make_test_array(4));
        // 4 emitted immediately (in order), then 5 released by contiguity;
        // blocking_recv below is the wait.
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 4);
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 5);
    }

    #[test]
    fn test_throttle_drops_output_arrays() {
        // G7: with a tiny MaxByteRate, output arrays exceeding the byte budget
        // are dropped and counted into DroppedOutputArrays.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "THROTTLE_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // MaxByteRate = 8 bytes/sec. Each test array is 4 bytes; the bucket
        // starts full at 8, so the first two pass and the rest are dropped.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.max_byte_rate, 0, 8.0)
            .unwrap();
        params_applied(&handle);

        for id in 1..=5 {
            send_array(handle.array_sender(), make_test_array(id));
        }
        // ArrayCounter counts every processed frame (throttle drops happen on
        // the output side, after counting), and its flush follows the array
        // publish — so counter == 5 means everything that will ever reach the
        // downstream queue is already there.
        let port = handle.port_runtime().port_handle().clone();
        let counter = handle.ndarray_params.array_counter;
        wait_until("all 5 frames to be processed", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 5)
        });

        // Drain whatever made it through — strictly fewer than 5.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut received = 0;
        while rt
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_millis(20), downstream_rx.recv())
                    .await
            })
            .map(|o| o.is_some())
            .unwrap_or(false)
        {
            received += 1;
        }
        assert!(
            received < 5,
            "throttle must drop some arrays (got {received})"
        );
        assert!(received >= 1, "first array within budget must pass");
    }

    #[test]
    fn test_process_plugin_reprocesses_last_input() {
        // G5: writing ProcessPlugin re-injects the cached last input array.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "PROCESS_PLUGIN_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(7));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 7);

        // Trigger ProcessPlugin — the cached input (id=7) is reprocessed.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        let reprocessed = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            reprocessed.unique_id, 7,
            "ProcessPlugin re-emits last input"
        );
    }

    #[test]
    fn test_min_callback_time_throttle_not_counted() {
        // A MinCallbackTime-throttled array is silently skipped, NOT counted.
        // C++ driverCallback (NDPluginDriver.cpp:405-450) falls through the
        // `deltaTime <= minCallbackTime` gate straight to callParamCallbacks()
        // without touching DroppedArrays — that counter is incremented ONLY on
        // a compression-unaware array (:388) or a full message queue (:440).
        // Verify (a) the throttled array is not emitted and (b) DroppedArrays
        // stays zero.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "MIN_CB_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);
        let dropped = handle.array_sender().dropped_arrays_counter().clone();

        // 10s minimum between callbacks — only the first array gets through.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 10.0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 1);
        // Fence the array queue: array 2 must have been consumed (throttled
        // out) before the negative checks below, or they could false-pass on
        // a not-yet-processed frame.
        params_applied(&handle);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let second = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            second.is_err(),
            "second array throttled out by MinCallbackTime"
        );
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "a MinCallbackTime-throttled frame must NOT increment DroppedArrays"
        );
    }

    #[test]
    fn test_array_callbacks_zero_withholds_downstream_delivery() {
        // ADC-2: NDArrayCallbacks==0 stops downstream NDArray delivery while
        // the plugin still processes and updates its metadata params — C++
        // endProcessCallbacks (NDPluginDriver.cpp:257-265) caches the array and
        // returns before doCallbacksGenericPointer. Distinct from
        // EnableCallbacks, which gates whether the plugin processes at all.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "ARRAY_CB_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle().clone();

        // Disable downstream array callbacks.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        // Processing fence: the counter flush follows any downstream publish,
        // so once it reads 1, a delivery that was going to happen already has.
        wait_until("frame 1 to be processed", || {
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .is_ok_and(|v| v == 1)
        });

        // No downstream delivery.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let got = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            got.is_err(),
            "NDArrayCallbacks=0 must withhold downstream delivery"
        );
        // But the plugin still processed: ArrayCounter advanced.
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            1,
            "processing (and metadata params) must continue while delivery is off"
        );

        // Re-enable: the next array IS delivered downstream.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);
        send_array(handle.array_sender(), make_test_array(2));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 2);
    }

    #[test]
    fn test_plugin_output_publishes_compressed_size() {
        // ADC-3: every processed array publishes NDCodec / NDCompressedSize
        // (C++ beginProcessCallbacks NDPluginDriver.cpp:213-214). An
        // uncompressed output carries an empty codec name and compressedSize ==
        // raw bytes; a compressed output carries the codec name and its
        // compressed size. CompressedSize_RBV (Int32) exercises both arms.
        struct CompressProcessor;
        impl NDPluginProcess for CompressProcessor {
            fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
                let mut out = array.clone();
                out.codec = Some(crate::codec::Codec {
                    name: crate::codec::CodecName::JPEG,
                    compressed_size: 7,
                    level: 0,
                    shuffle: 0,
                    compressor: 0,
                    original_data_type: NDDataType::UInt8,
                });
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            fn plugin_type(&self) -> &str {
                "Compress"
            }
        }

        // Uncompressed passthrough: a 4-byte UInt8 array → compressedSize == 4.
        {
            let pool = Arc::new(NDArrayPool::new(1_000_000));
            let (ds, _rx) = ndarray_channel("DS_RAW", 10);
            let mut output = NDArrayOutput::new();
            output.add(ds);
            let (handle, _jh) = create_plugin_runtime_with_output(
                "CODEC_RAW",
                PassthroughProcessor,
                pool,
                10,
                output,
                "",
                test_wiring(),
            );
            enable_callbacks(&handle);
            let port = handle.port_runtime().port_handle().clone();
            send_array(handle.array_sender(), make_test_array(1));
            // The read errors with ParamUndefined until the first flush — treat
            // that as "not yet".
            wait_until(
                "uncompressed output to publish CompressedSize == raw byte count",
                || {
                    port.read_int32_blocking(handle.ndarray_params.compressed_size, 0)
                        .is_ok_and(|v| v == 4)
                },
            );
        }

        // Compressed output: compressedSize == codec.compressed_size (7).
        {
            let pool = Arc::new(NDArrayPool::new(1_000_000));
            let (ds, _rx) = ndarray_channel("DS_CMP", 10);
            let mut output = NDArrayOutput::new();
            output.add(ds);
            let (handle, _jh) = create_plugin_runtime_with_output(
                "CODEC_CMP",
                CompressProcessor,
                pool,
                10,
                output,
                "",
                test_wiring(),
            );
            enable_callbacks(&handle);
            let port = handle.port_runtime().port_handle().clone();
            send_array(handle.array_sender(), make_test_array(1));
            wait_until(
                "compressed output to publish CompressedSize == codec.compressed_size",
                || {
                    port.read_int32_blocking(handle.ndarray_params.compressed_size, 0)
                        .is_ok_and(|v| v == 7)
                },
            );
        }
    }

    #[test]
    fn test_process_plugin_skips_throttled_input() {
        // a MinCallbackTime-throttled frame must NOT be cached as the
        // ProcessPlugin input. After array 1 is processed and array 2 is
        // dropped by the throttle, ProcessPlugin must re-inject array 1
        // (the last *processed* array), not the dropped array 2.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "PROCESS_THROTTLE_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // 10s minimum between callbacks — only the first array is processed.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 10.0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        // Array 1 was processed and emitted; array 2 was throttled out.
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 1);
        // Fence the ARRAY queue: array 2 must be consumed (and throttled out)
        // under the 10s gate before the reset below un-gates it. The barrier
        // acks only once the array queue has drained.
        params_applied(&handle);

        // ProcessPlugin re-injects the cached input. The cache must still hold
        // array 1, because array 2 never passed the throttle gate. The
        // re-injected array itself is also subject to the throttle, so reset
        // MinCallbackTime to 0 first so the re-injected frame is processed.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 0.0)
            .unwrap();
        // No fence needed: the MinCallbackTime reset and the ProcessPlugin
        // trigger below travel the same FIFO param channel, so the reset is
        // applied before the trigger by construction.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        let reprocessed = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            reprocessed.unique_id, 1,
            "ProcessPlugin must re-inject the last processed array (1), not the throttled array (2)"
        );
    }

    #[test]
    fn test_g3_compressed_array_dropped_on_non_aware_plugin() {
        // G3: a non-compression-aware plugin drops a compressed array.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "G3_TEST",
            PassthroughProcessor, // compression_aware() defaults to false
            pool,
            10,
            output,
            "",
            test_wiring(),
        );
        enable_callbacks(&handle);

        // A compressed array must be dropped, not forwarded.
        let mut compressed = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        compressed.unique_id = 1;
        compressed.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::JPEG,
            compressed_size: 16,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt8,
        });
        send_array(handle.array_sender(), Arc::new(compressed));

        // An uncompressed array passes through normally.
        send_array(handle.array_sender(), make_test_array(2));

        let r = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            r.unique_id, 2,
            "compressed array dropped; only the raw array reaches downstream"
        );
    }

    #[test]
    fn test_drop_on_full_increments_dropped_counter() {
        // B1/G1: a slow downstream plugin with a tiny input queue drops arrays
        // when the queue is full; the drop is counted in the plugin's shared
        // DroppedArrays counter rather than back-pressuring the producer.
        struct SlowProcessor;
        impl NDPluginProcess for SlowProcessor {
            fn process_array(&mut self, _a: &NDArray, _p: &NDArrayPool) -> ProcessResult {
                std::thread::sleep(std::time::Duration::from_millis(200));
                ProcessResult::empty()
            }
            fn plugin_type(&self) -> &str {
                "Slow"
            }
        }
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        // Downstream plugin with queue size 1 and a slow processor.
        let (downstream_handle, _ds_jh) =
            create_plugin_runtime("B1_DOWNSTREAM", SlowProcessor, pool, 1, "", test_wiring());
        enable_callbacks(&downstream_handle);
        let ds_sender = downstream_handle.array_sender().clone();
        let dropped = ds_sender.dropped_arrays_counter().clone();

        // First array is taken by the data loop (now sleeping 200ms); second
        // fills the 1-slot queue; the rest find a full queue → dropped.
        send_array(&ds_sender, make_test_array(1));
        send_array(&ds_sender, make_test_array(2));
        send_array(&ds_sender, make_test_array(3));
        send_array(&ds_sender, make_test_array(4));

        assert!(
            dropped.load(Ordering::Acquire) >= 1,
            "arrays dropped on a full queue must be counted (got {})",
            dropped.load(Ordering::Acquire)
        );
    }

    #[test]
    fn test_cross_width_narrowing_array_read_truncates() {
        // Cross-width integer narrowing array reads must TRUNCATE (wrapping),
        // matching the C cast in C++ NDArrayPool.cpp:388 `convertType`
        //   *pDataOut++ = (dataTypeOut)(*pDataIn++);
        // A C cast `(epicsInt8)(epicsUInt16)300` keeps the low 8 bits == 44.
        // The f64 round-trip in copy_convert would SATURATE (`300.0 as i8`
        // == 127) and diverge from C++ — copy_ccast must be used instead.

        // U16 -> i8: 300 = 0x012C; low byte 0x2C = 44.
        let mut out = [0i8; 1];
        let n = copy_ccast(&[300u16], &mut out);
        assert_eq!(n, 1);
        assert_eq!(out[0], 44, "(epicsInt8)(epicsUInt16)300 == 44 (low 8 bits)");
        // copy_convert would have saturated:
        let mut sat = [0i8; 1];
        copy_convert(&[300u16], &mut sat);
        assert_eq!(sat[0], 127, "f64 round-trip saturates — the wrong behavior");

        // I32 -> i8: 0x1234_5678 -> low byte 0x78 = 120.
        let mut out2 = [0i8; 1];
        copy_ccast(&[0x1234_5678i32], &mut out2);
        assert_eq!(out2[0], 0x78);

        // I32 -> i8: -1 stays -1 (all-ones low byte).
        let mut out3 = [0i8; 1];
        copy_ccast(&[-1i32], &mut out3);
        assert_eq!(out3[0], -1);

        // U16 -> i8: 0x00FF = 255 -> low byte 0xFF reinterpreted as i8 == -1.
        let mut out4 = [0i8; 1];
        copy_ccast(&[255u16], &mut out4);
        assert_eq!(out4[0], -1);

        // I64 -> i32: 0x0000_0001_0000_002A -> low 32 bits == 42.
        let mut out5 = [0i32; 1];
        copy_ccast(&[0x0000_0001_0000_002Ai64], &mut out5);
        assert_eq!(out5[0], 42);

        // U32 -> i16: 70000 = 0x0001_1170 -> low 16 bits 0x1170 == 4464.
        let mut out6 = [0i16; 1];
        copy_ccast(&[70000u32], &mut out6);
        assert_eq!(out6[0], 4464);

        // Same-width sign change still works as a bitwise reinterpret:
        // U8 255 -> i8 -1.
        let mut out7 = [0i8; 1];
        copy_ccast(&[255u8], &mut out7);
        assert_eq!(out7[0], -1);

        // F64 out-of-range -> i32 still routes through copy_convert (the
        // `convert:` arm for float sources). C++ converts float->int with a
        // C cast too, but the runtime keeps the f64 numeric path for float
        // sources; this asserts the integer-narrowing fix did not change the
        // float-source path.
        let mut fout = [0i32; 1];
        copy_convert(&[42.9f64], &mut fout);
        assert_eq!(fout[0], 42, "f64 -> i32 truncates toward zero");
    }

    // ---- ADP-45: scatter overflow-reroute (C++ NDPluginScatter) ----

    /// Run an async body on a throwaway current-thread runtime.
    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn test_scatter_reroutes_past_full_consumer() {
        // 3 consumers, queue size 1. Pre-fill A so its queue is full; a scatter
        // that would target A must reroute to B (C++ auxStatus=asynOverflow),
        // and the rerouted-away full queue must NOT count a dropped array
        // (driverCallback ignoreQueueFull, NDPluginDriver.cpp:406,433-442).
        let (sa, mut ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 1);
        let (sc, _rc) = ndarray_channel("C", 1);
        block(async {
            assert_eq!(
                sa.publish(make_test_array(99)).await,
                PublishOutcome::Delivered
            );
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            // A rerouted (attempt 0), B delivered (attempt 1): cursor +2.
            assert_eq!(cursor, 2);
            assert_eq!(rb.recv().await.unwrap().unique_id, 1);
            // A still holds only its filler; the rerouted-away drop was not counted.
            assert_eq!(ra.recv().await.unwrap().unique_id, 99);
            assert_eq!(sa.dropped_arrays_counter().load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn test_scatter_drops_on_last_when_all_full_counts_once() {
        // Both consumers full. The array is dropped on the last node only, and
        // the drop is counted exactly once (C++ sets auxStatus=asynSuccess for
        // the last node so its full queue drops and counts).
        let (sa, mut ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 1);
        block(async {
            sa.publish(make_test_array(91)).await;
            sb.publish(make_test_array(92)).await;
            let senders = vec![sa.clone(), sb.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(7), &senders, &mut cursor).await;
            assert_eq!(cursor, 2); // both attempted
            // A rerouted-away (not counted); B last (dropped, counted once).
            assert_eq!(sa.dropped_arrays_counter().load(Ordering::Acquire), 0);
            assert_eq!(sb.dropped_arrays_counter().load(Ordering::Acquire), 1);
            // Neither queue received frame 7 — both still hold their fillers.
            assert_eq!(ra.recv().await.unwrap().unique_id, 91);
            assert_eq!(rb.recv().await.unwrap().unique_id, 92);
        });
    }

    #[test]
    fn test_scatter_cursor_advances_per_attempt_across_frames() {
        // A is permanently full; B and C are free. Frame 0 reroutes A->B, so
        // the persistent cursor (C++ nextClient_) ends past B. Frame 1 must
        // therefore start at C, NOT back at B: a per-frame cursor would send
        // frame 1 to B; the per-attempt cursor sends it to C.
        let (sa, _ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 10);
        let (sc, mut rc) = ndarray_channel("C", 10);
        block(async {
            sa.publish(make_test_array(90)).await; // fill A permanently
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(0), &senders, &mut cursor).await;
            assert_eq!(cursor, 2); // A(reroute) + B(deliver)
            assert_eq!(rb.recv().await.unwrap().unique_id, 0);
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            assert_eq!(cursor, 3); // C(deliver) on the first attempt
            assert_eq!(rc.recv().await.unwrap().unique_id, 1);
        });
    }

    #[test]
    fn test_scatter_skips_disabled_consumer() {
        // A disabled downstream is unregistered from the interrupt list in C++
        // (setArrayInterrupt(0)) and must not consume a round-robin slot.
        let (sa, mut ra) = ndarray_channel("A", 10);
        let (mut sb, _rb) = ndarray_channel("B", 10);
        let (sc, mut rc) = ndarray_channel("C", 10);
        sb.set_mode_flags(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        block(async {
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            // Active set = [A, C] (n=2): frame 0 -> A, frame 1 -> C.
            ProcessOutput::scatter_publish(&make_test_array(0), &senders, &mut cursor).await;
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            assert_eq!(ra.recv().await.unwrap().unique_id, 0);
            assert_eq!(rc.recv().await.unwrap().unique_id, 1);
        });
    }
}
