use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::user::AsynUser;

use asyn_rs::port_handle::PortHandle;

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;

use super::channel::{NDArrayOutput, NDArrayReceiver, NDArraySender, ndarray_channel};
use super::params::PluginBaseParams;
use super::wiring::WiringRegistry;

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
}

/// Result of processing one array: output arrays + param updates to write back.
pub struct ProcessResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
    /// If set, only publish to the subscriber at this index (round-robin scatter).
    pub scatter_index: Option<usize>,
}

impl ProcessResult {
    /// Convenience: sink plugin with only param updates, no output arrays.
    pub fn sink(param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![],
            param_updates,
            scatter_index: None,
        }
    }

    /// Convenience: passthrough/transform plugin with output arrays but no param updates.
    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            scatter_index: None,
        }
    }

    /// Convenience: no outputs, no param updates.
    pub fn empty() -> Self {
        Self {
            output_arrays: vec![],
            param_updates: vec![],
            scatter_index: None,
        }
    }

    /// Convenience: scatter output — send to a single subscriber by index.
    pub fn scatter(output_arrays: Vec<Arc<NDArray>>, index: usize) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            scatter_index: Some(index),
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

/// Sort buffer for reordering output arrays by uniqueId.
///
/// When sort_mode is enabled, output arrays are inserted into a BTreeMap
/// keyed by uniqueId instead of being sent directly. A periodic flush task
/// drains arrays in uniqueId order.
struct SortBuffer {
    /// Buffered arrays keyed by uniqueId, ordered by BTreeMap.
    entries: BTreeMap<i32, Vec<Arc<NDArray>>>,
    /// The last uniqueId that was emitted (for detecting disordered arrays).
    last_emitted_id: i32,
    /// Counter of arrays received out of order (uniqueId < last_emitted_id).
    disordered_arrays: i32,
    /// Counter of arrays dropped because the buffer was full.
    dropped_output_arrays: i32,
}

impl SortBuffer {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            last_emitted_id: 0,
            disordered_arrays: 0,
            dropped_output_arrays: 0,
        }
    }

    /// Insert arrays into the sort buffer. If buffer exceeds sort_size, drop oldest entries.
    fn insert(&mut self, unique_id: i32, arrays: Vec<Arc<NDArray>>, sort_size: i32) {
        if unique_id < self.last_emitted_id {
            self.disordered_arrays += 1;
        }
        self.entries.entry(unique_id).or_default().extend(arrays);

        // Enforce sort_size limit by dropping oldest entries
        while sort_size > 0 && self.entries.len() as i32 > sort_size {
            if let Some((&oldest_key, _)) = self.entries.iter().next() {
                self.entries.remove(&oldest_key);
                self.dropped_output_arrays += 1;
            }
        }
    }

    /// Drain all buffered arrays in uniqueId order. Returns them as (uniqueId, arrays) pairs.
    fn drain_all(&mut self) -> Vec<(i32, Vec<Arc<NDArray>>)> {
        let entries: Vec<_> = std::mem::take(&mut self.entries).into_iter().collect();
        if let Some(&(last_id, _)) = entries.last() {
            self.last_emitted_id = last_id;
        }
        entries
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
    array_counter: i32,
    /// Param index for STD_ARRAY_DATA (if this is a StdArrays plugin).
    std_array_data_param: Option<usize>,
    /// MinCallbackTime throttling: minimum seconds between process calls.
    min_callback_time: f64,
    /// Last time process_and_publish was called (for throttling).
    last_process_time: Option<std::time::Instant>,
    /// Sort mode: 0 = disabled, 1 = sorted output.
    sort_mode: i32,
    /// Sort time: seconds between periodic flushes of the sort buffer.
    sort_time: f64,
    /// Sort size: maximum number of uniqueId entries in the sort buffer.
    sort_size: i32,
    /// Sort buffer for reordering output arrays by uniqueId.
    sort_buffer: SortBuffer,
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

    /// Process array and return a `ProcessOutput`. Does NOT send to actor.
    /// Direct interrupts (std_array_data_param) happen here (sync).
    /// The returned output must be published and flushed by the caller in async context.
    fn process_and_publish(&mut self, array: &NDArray) -> Option<ProcessOutput> {
        if self.should_throttle() {
            return None;
        }
        let t0 = std::time::Instant::now();
        let result = self.processor.process_array(array, &self.pool);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.last_process_time = Some(t0);

        if self.sort_mode != 0 && !result.output_arrays.is_empty() {
            let unique_id = array.unique_id;
            self.sort_buffer
                .insert(unique_id, result.output_arrays, self.sort_size);
            let mut batch = self.build_sort_params_batch();
            if !result.param_updates.is_empty() {
                let frame_output = self.build_publish_batch(
                    vec![],
                    result.param_updates,
                    result.scatter_index,
                    Some(array),
                    elapsed_ms,
                );
                batch.merge(frame_output.batch);
            }
            Some(ProcessOutput {
                arrays: vec![],
                scatter_index: None,
                batch,
            })
        } else {
            Some(self.build_publish_batch(
                result.output_arrays,
                result.param_updates,
                result.scatter_index,
                Some(array),
                elapsed_ms,
            ))
        }
    }

    /// Flush the sort buffer: drain all arrays in uniqueId order.
    fn flush_sort_buffer(&mut self) -> ProcessOutput {
        let entries = self.sort_buffer.drain_all();
        let mut all_arrays = Vec::new();
        let mut combined = ParamBatch::empty();
        for (_unique_id, arrays) in entries {
            let output = self.build_publish_batch(arrays, vec![], None, None, 0.0);
            all_arrays.extend(output.arrays);
            combined.merge(output.batch);
        }
        combined.merge(self.build_sort_params_batch());
        ProcessOutput {
            arrays: all_arrays,
            scatter_index: None,
            batch: combined,
        }
    }

    fn build_sort_params_batch(&self) -> ParamBatch {
        use asyn_rs::request::ParamSetValue;
        let sort_free = self.sort_size - self.sort_buffer.len();
        ParamBatch {
            addr0: vec![
                ParamSetValue::Int32 {
                    reason: self.plugin_params.sort_free,
                    addr: 0,
                    value: sort_free,
                },
                ParamSetValue::Int32 {
                    reason: self.plugin_params.disordered_arrays,
                    addr: 0,
                    value: self.sort_buffer.disordered_arrays,
                },
                ParamSetValue::Int32 {
                    reason: self.plugin_params.dropped_output_arrays,
                    addr: 0,
                    value: self.sort_buffer.dropped_output_arrays,
                },
            ],
            extra: std::collections::HashMap::new(),
        }
    }

    /// Build a ProcessOutput: fires direct interrupts (sync) and collects
    /// param updates into a batch. Does NOT publish arrays — the caller
    /// must publish them in async context.
    fn build_publish_batch(
        &mut self,
        output_arrays: Vec<Arc<NDArray>>,
        param_updates: Vec<ParamUpdate>,
        scatter_index: Option<usize>,
        fallback_array: Option<&NDArray>,
        elapsed_ms: f64,
    ) -> ProcessOutput {
        use asyn_rs::request::ParamSetValue;

        let mut addr0: Vec<ParamSetValue> = Vec::new();
        let mut extra: std::collections::HashMap<i32, Vec<ParamSetValue>> =
            std::collections::HashMap::new();

        if let Some(report_arr) = output_arrays.first().map(|a| a.as_ref()).or(fallback_array) {
            self.array_counter += 1;

            // Fire array data interrupt directly (C EPICS pattern).
            if let Some(param) = self.std_array_data_param {
                use crate::ndarray::NDDataBuffer;
                use asyn_rs::param::ParamValue;
                let value = match &report_arr.data {
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
                    let ts = report_arr.timestamp.to_system_time();
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
            let color_mode = if report_arr.dims.len() <= 2 { 0 } else { 2 };
            addr0.extend([
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.array_counter,
                    addr: 0,
                    value: self.array_counter,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.unique_id,
                    addr: 0,
                    value: report_arr.unique_id,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.n_dimensions,
                    addr: 0,
                    value: report_arr.dims.len() as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.array_size_x,
                    addr: 0,
                    value: info.x_size as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.array_size_y,
                    addr: 0,
                    value: info.y_size as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.array_size_z,
                    addr: 0,
                    value: info.color_size as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.array_size,
                    addr: 0,
                    value: info.total_bytes as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.data_type,
                    addr: 0,
                    value: report_arr.data.data_type() as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.color_mode,
                    addr: 0,
                    value: color_mode,
                },
                ParamSetValue::Float64 {
                    reason: self.ndarray_params.timestamp_rbv,
                    addr: 0,
                    value: report_arr.timestamp.as_f64(),
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.epics_ts_sec,
                    addr: 0,
                    value: report_arr.timestamp.sec as i32,
                },
                ParamSetValue::Int32 {
                    reason: self.ndarray_params.epics_ts_nsec,
                    addr: 0,
                    value: report_arr.timestamp.nsec as i32,
                },
            ]);
        }

        addr0.push(ParamSetValue::Float64 {
            reason: self.plugin_params.execution_time,
            addr: 0,
            value: elapsed_ms,
        });

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
                    let pv = ParamSetValue::Int32 {
                        reason: *reason,
                        addr: *addr,
                        value: *value,
                    };
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
                    let pv = ParamSetValue::Float64 {
                        reason: *reason,
                        addr: *addr,
                        value: *value,
                    };
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
                    let pv = ParamSetValue::Octet {
                        reason: *reason,
                        addr: *addr,
                        value: value.clone(),
                    };
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
                    let pv = ParamSetValue::Float64Array {
                        reason: *reason,
                        addr: *addr,
                        value: value.clone(),
                    };
                    if *addr == 0 {
                        addr0.push(pv);
                    } else {
                        extra.entry(*addr).or_default().push(pv);
                    }
                }
            }
        }

        ProcessOutput {
            arrays: output_arrays,
            scatter_index,
            batch: ParamBatch { addr0, extra },
        }
    }
}

/// Output from processing: arrays to publish + param batch to flush.
struct ProcessOutput {
    arrays: Vec<Arc<NDArray>>,
    scatter_index: Option<usize>,
    batch: ParamBatch,
}

impl ProcessOutput {
    /// Publish arrays to downstream senders (async, concurrent fan-out).
    ///
    /// Each array is published to all senders concurrently (independent
    /// backpressure per sender). Arrays are published in order — the next
    /// array's fan-out starts only after the previous one completes.
    async fn publish_arrays(&self, senders: &[NDArraySender]) {
        for arr in &self.arrays {
            if let Some(idx) = self.scatter_index {
                if let Some(sender) = senders.get(idx % senders.len().max(1)) {
                    sender.publish(arr.clone()).await;
                }
            } else {
                let futs = senders.iter().map(|s| s.publish(arr.clone()));
                futures_util::future::join_all(futs).await;
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

/// PortDriver implementation for a plugin's control plane.
#[allow(dead_code)]
pub struct PluginPortDriver {
    base: PortDriverBase,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    param_change_tx: tokio::sync::mpsc::Sender<(usize, i32, ParamChangeValue)>,
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
        param_change_tx: tokio::sync::mpsc::Sender<(usize, i32, ParamChangeValue)>,
        processor: &mut P,
        array_data: Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>>,
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

        // Set defaults (EnableCallbacks=0: Disable by default, matching EPICS ADCore)
        base.set_int32_param(plugin_params.enable_callbacks, 0, 0)?;
        base.set_int32_param(plugin_params.blocking_callbacks, 0, 0)?;
        base.set_int32_param(plugin_params.queue_size, 0, queue_size as i32)?;
        base.set_int32_param(plugin_params.dropped_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.queue_use, 0, 0)?;
        base.set_string_param(plugin_params.plugin_type, 0, plugin_type_name.into())?;
        base.set_int32_param(ndarray_params.array_callbacks, 0, 1)?;
        base.set_int32_param(ndarray_params.write_file, 0, 0)?;
        base.set_int32_param(ndarray_params.read_file, 0, 0)?;
        base.set_int32_param(ndarray_params.capture, 0, 0)?;
        base.set_int32_param(ndarray_params.file_write_status, 0, 0)?;
        base.set_string_param(ndarray_params.file_write_message, 0, "".into())?;
        base.set_string_param(ndarray_params.file_path, 0, "".into())?;
        base.set_string_param(ndarray_params.file_name, 0, "".into())?;
        base.set_int32_param(ndarray_params.file_number, 0, 0)?;
        base.set_int32_param(ndarray_params.auto_increment, 0, 0)?;
        base.set_string_param(ndarray_params.file_template, 0, "%s%s_%3.3d.dat".into())?;
        base.set_string_param(ndarray_params.full_file_name, 0, "".into())?;
        base.set_int32_param(ndarray_params.create_dir, 0, 0)?;
        base.set_string_param(ndarray_params.temp_suffix, 0, "".into())?;

        // Set plugin identity params
        base.set_string_param(ndarray_params.port_name_self, 0, port_name.into())?;
        base.set_string_param(
            ndarray_params.ad_core_version,
            0,
            env!("CARGO_PKG_VERSION").into(),
        )?;
        base.set_string_param(
            ndarray_params.driver_version,
            0,
            env!("CARGO_PKG_VERSION").into(),
        )?;
        if !ndarray_port.is_empty() {
            base.set_string_param(plugin_params.nd_array_port, 0, ndarray_port.into())?;
        }

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
    ($self:expr, $buf:expr, $direct_variant:ident, $( $variant:ident ),*) => {{
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
        let _ = self
            .param_change_tx
            .try_send((reason, addr, ParamChangeValue::Int32(value)));
        Ok(())
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        let addr = user.addr;
        self.base.set_float64_param(reason, addr, value)?;
        self.base.call_param_callbacks(addr)?;
        let _ = self
            .param_change_tx
            .try_send((reason, addr, ParamChangeValue::Float64(value)));
        Ok(())
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        let reason = user.reason;
        let addr = user.addr;
        let s = String::from_utf8_lossy(data).into_owned();
        self.base.set_string_param(reason, addr, s.clone())?;
        self.base.call_param_callbacks(addr)?;
        let _ = self
            .param_change_tx
            .try_send((reason, addr, ParamChangeValue::Octet(s)));
        Ok(())
    }

    fn read_int8_array(&mut self, _user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize> {
        impl_read_array!(self, buf, I8, U8, I16, U16, I32, U32, I64, U64, F32, F64)
    }

    fn read_int16_array(&mut self, _user: &AsynUser, buf: &mut [i16]) -> AsynResult<usize> {
        impl_read_array!(self, buf, I16, I8, U8, U16, I32, U32, I64, U64, F32, F64)
    }

    fn read_int32_array(&mut self, _user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        impl_read_array!(self, buf, I32, I8, U8, I16, U16, U32, I64, U64, F32, F64)
    }

    fn read_float32_array(&mut self, _user: &AsynUser, buf: &mut [f32]) -> AsynResult<usize> {
        impl_read_array!(self, buf, F32, I8, U8, I16, U16, I32, U32, I64, U64, F64)
    }

    fn read_float64_array(&mut self, _user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        impl_read_array!(self, buf, F64, I8, U8, I16, U16, I32, U32, I64, U64, F32)
    }
}

/// Handle to a running plugin runtime. Provides access to sender and port handle.
#[derive(Clone)]
pub struct PluginRuntimeHandle {
    port_runtime: PortRuntimeHandle,
    array_sender: NDArraySender,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    port_name: String,
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
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<(usize, i32, ParamChangeValue)>(64);

    // Capture plugin type and array data handle before mutable borrow
    let plugin_type_name = processor.plugin_type().to_string();
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
    )
    .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let blocking_callbacks_reason = driver.plugin_params.blocking_callbacks;
    let min_callback_time_reason = driver.plugin_params.min_callback_time;
    let sort_mode_reason = driver.plugin_params.sort_mode;
    let sort_time_reason = driver.plugin_params.sort_time;
    let sort_size_reason = driver.plugin_params.sort_size;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;
    let std_array_data_param = driver.std_array_data_param;

    // Create port runtime (actor thread for param I/O)
    let (port_runtime, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default());

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
    let shared = Arc::new(parking_lot::Mutex::new(SharedProcessorInner {
        processor,
        output: array_output,
        pool,
        ndarray_params,
        plugin_params,
        port_handle,
        array_counter: 0,
        std_array_data_param,
        min_callback_time: 0.0,
        last_process_time: None,
        sort_mode: 0,
        sort_time: 0.0,
        sort_size: 10,
        sort_buffer: SortBuffer::new(),
    }));

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();

    let mut array_sender = array_sender;
    array_sender.set_mode_flags(enabled, blocking_mode);

    // Capture wiring info for data loop
    let nd_array_port_reason = plugin_params.nd_array_port;
    let sender_port_name = port_name.to_string();
    let initial_upstream = ndarray_port.to_string();

    // Spawn data processing thread
    let data_jh = thread::Builder::new()
        .name(format!("plugin-data-{port_name}"))
        .spawn(move || {
            plugin_data_loop(
                shared,
                array_rx,
                param_rx,
                enable_callbacks_reason,
                blocking_callbacks_reason,
                min_callback_time_reason,
                sort_mode_reason,
                sort_time_reason,
                sort_size_reason,
                data_enabled,
                data_blocking,
                nd_array_port_reason,
                sender_port_name,
                initial_upstream,
                wiring,
            );
        })
        .expect("failed to spawn plugin data thread");

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        array_output: array_output_for_handle,
        port_name: port_name.to_string(),
        ndarray_params,
        plugin_params,
    };

    (handle, data_jh)
}

fn plugin_data_loop<P: NDPluginProcess>(
    shared: Arc<parking_lot::Mutex<SharedProcessorInner<P>>>,
    mut array_rx: NDArrayReceiver,
    mut param_rx: tokio::sync::mpsc::Receiver<(usize, i32, ParamChangeValue)>,
    enable_callbacks_reason: usize,
    blocking_callbacks_reason: usize,
    min_callback_time_reason: usize,
    sort_mode_reason: usize,
    sort_time_reason: usize,
    sort_size_reason: usize,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    nd_array_port_reason: usize,
    sender_port_name: String,
    initial_upstream: String,
    wiring: Arc<WiringRegistry>,
) {
    let mut current_upstream = initial_upstream;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Sort flush timer — starts disabled (very long interval).
        // Re-created when sort_time changes.
        let mut sort_flush_interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        let mut sort_flush_active = false;

        loop {
            tokio::select! {
                msg = array_rx.recv_msg() => {
                    match msg {
                        Some(msg) => {
                            // Process array and collect output (sync, under lock).
                            let (process_output, senders, port) = {
                                let mut guard = shared.lock();
                                let output = guard.process_and_publish(&msg.array);
                                let senders = guard.output.lock().senders_clone();
                                let port = guard.port_handle.clone();
                                (output, senders, port)
                            };
                            // msg dropped here → completion signaled (if tracked)
                            // Publish arrays and flush params outside the lock, in async context.
                            if let Some(po) = process_output {
                                po.publish_arrays(&senders).await;
                                po.batch.flush(&port).await;
                            }
                        }
                        None => break,
                    }
                }
                param = param_rx.recv() => {
                    match param {
                        Some((reason, addr, value)) => {
                            if reason == enable_callbacks_reason {
                                enabled.store(value.as_i32() != 0, Ordering::Release);
                            }
                            if reason == blocking_callbacks_reason {
                                blocking_mode.store(value.as_i32() != 0, Ordering::Release);
                            }
                            // Handle MinCallbackTime param change
                            if reason == min_callback_time_reason {
                                shared.lock().min_callback_time = value.as_f64();
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
                                            let dur = std::time::Duration::from_secs_f64(guard.sort_time);
                                            sort_flush_interval = tokio::time::interval(dur);
                                        }
                                        None
                                    }
                                };
                                if let Some((output, senders, port)) = flush_work {
                                    output.publish_arrays(&senders).await;
                                    output.batch.flush(&port).await;
                                }
                            }
                            if reason == sort_time_reason {
                                let t = value.as_f64();
                                let mut guard = shared.lock();
                                guard.sort_time = t;
                                if guard.sort_mode != 0 && t > 0.0 {
                                    sort_flush_active = true;
                                    let dur = std::time::Duration::from_secs_f64(t);
                                    sort_flush_interval = tokio::time::interval(dur);
                                } else {
                                    sort_flush_active = false;
                                }
                                drop(guard);
                            }
                            if reason == sort_size_reason {
                                shared.lock().sort_size = value.as_i32();
                            }
                            // Handle NDArrayPort rewiring
                            if reason == nd_array_port_reason {
                                if let Some(new_port) = value.as_string() {
                                    if new_port != current_upstream {
                                        let old = std::mem::replace(&mut current_upstream, new_port.to_string());
                                        if let Err(e) = wiring.rewire_by_name(&sender_port_name, &old, new_port) {
                                            eprintln!("NDArrayPort rewire failed: {e}");
                                            current_upstream = old;
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
                                    Some(guard.build_publish_batch(result.output_arrays, result.param_updates, None, None, elapsed_ms))
                                } else {
                                    None
                                };
                                let senders = guard.output.lock().senders_clone();
                                (output, senders, guard.port_handle.clone())
                            };
                            if let Some(po) = process_output {
                                po.publish_arrays(&senders).await;
                                po.batch.flush(&port).await;
                            }
                        }
                        None => break,
                    }
                }
                _ = sort_flush_interval.tick(), if sort_flush_active => {
                    let (output, senders, port) = {
                        let mut guard = shared.lock();
                        let output = guard.flush_sort_buffer();
                        let senders = guard.output.lock().senders_clone();
                        let port = guard.port_handle.clone();
                        (output, senders, port)
                    };
                    output.publish_arrays(&senders).await;
                    output.batch.flush(&port).await;
                }
            }
        }
    });
}

/// Connect a downstream plugin's sender to a plugin runtime's output.
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
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<(usize, i32, ParamChangeValue)>(64);

    let plugin_type_name = processor.plugin_type().to_string();
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
    )
    .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let blocking_callbacks_reason = driver.plugin_params.blocking_callbacks;
    let min_callback_time_reason = driver.plugin_params.min_callback_time;
    let sort_mode_reason = driver.plugin_params.sort_mode;
    let sort_time_reason = driver.plugin_params.sort_time;
    let sort_size_reason = driver.plugin_params.sort_size;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;
    let std_array_data_param = driver.std_array_data_param;

    let (port_runtime, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default());

    let port_handle = port_runtime.port_handle().clone();

    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    let enabled = Arc::new(AtomicBool::new(false));
    let blocking_mode = Arc::new(AtomicBool::new(false));

    let array_output = Arc::new(parking_lot::Mutex::new(output));
    let array_output_for_handle = array_output.clone();
    let shared = Arc::new(parking_lot::Mutex::new(SharedProcessorInner {
        processor,
        output: array_output,
        pool,
        ndarray_params,
        plugin_params,
        port_handle,
        array_counter: 0,
        std_array_data_param,
        min_callback_time: 0.0,
        last_process_time: None,
        sort_mode: 0,
        sort_time: 0.0,
        sort_size: 10,
        sort_buffer: SortBuffer::new(),
    }));

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();

    let mut array_sender = array_sender;
    array_sender.set_mode_flags(enabled, blocking_mode);

    // Capture wiring info for data loop
    let nd_array_port_reason = plugin_params.nd_array_port;
    let sender_port_name = port_name.to_string();
    let initial_upstream = ndarray_port.to_string();

    let data_jh = thread::Builder::new()
        .name(format!("plugin-data-{port_name}"))
        .spawn(move || {
            plugin_data_loop(
                shared,
                array_rx,
                param_rx,
                enable_callbacks_reason,
                blocking_callbacks_reason,
                min_callback_time_reason,
                sort_mode_reason,
                sort_time_reason,
                sort_size_reason,
                data_enabled,
                data_blocking,
                nd_array_port_reason,
                sender_port_name,
                initial_upstream,
                wiring,
            );
        })
        .expect("failed to spawn plugin data thread");

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        array_output: array_output_for_handle,
        port_name: port_name.to_string(),
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

    /// Enable callbacks on a plugin handle (plugins default to disabled).
    fn enable_callbacks(handle: &PluginRuntimeHandle) {
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
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

        // Give processing thread time
        std::thread::sleep(std::time::Duration::from_millis(50));

        // No crash, no output needed
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
        std::thread::sleep(std::time::Duration::from_millis(50));

        send_array(handle.array_sender(), make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Switch back to non-blocking
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

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
        std::thread::sleep(std::time::Duration::from_millis(50));

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
        std::thread::sleep(std::time::Duration::from_millis(50));

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
        assert_eq!(buf.last_emitted_id, 3);
    }

    #[test]
    fn test_sort_buffer_detects_disordered() {
        let mut buf = SortBuffer::new();

        // Emit id=5, then insert id=3 (which is less than last_emitted_id)
        buf.insert(5, vec![make_test_array(5)], 10);
        buf.drain_all(); // emits id=5, last_emitted_id=5

        buf.insert(3, vec![make_test_array(3)], 10);
        assert_eq!(buf.disordered_arrays, 1);
    }

    #[test]
    fn test_sort_buffer_drops_when_full() {
        let mut buf = SortBuffer::new();

        // sort_size=2, insert 3 entries
        buf.insert(1, vec![make_test_array(1)], 2);
        buf.insert(2, vec![make_test_array(2)], 2);
        buf.insert(3, vec![make_test_array(3)], 2);

        // Buffer should have 2 entries (oldest dropped)
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.dropped_output_arrays, 1);

        let drained = buf.drain_all();
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![2, 3], "oldest (id=1) should have been dropped");
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

        // Enable sort mode with sort_size=10
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_size, 0, 10)
            .unwrap();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_mode, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Send arrays out of order
        send_array(handle.array_sender(), make_test_array(3));
        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Arrays should be buffered, not yet received
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            result.is_err(),
            "arrays should be buffered while sort mode is active"
        );

        // Disable sort mode — should flush all buffered arrays in order
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_mode, 0, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Receive all flushed arrays — they should arrive in sorted order
        let r1 = downstream_rx.blocking_recv().unwrap();
        let r2 = downstream_rx.blocking_recv().unwrap();
        let r3 = downstream_rx.blocking_recv().unwrap();
        assert_eq!(r1.unique_id, 1);
        assert_eq!(r2.unique_id, 2);
        assert_eq!(r3.unique_id, 3);
    }
}
