use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{create_port_runtime, PortRuntimeHandle};
use asyn_rs::user::AsynUser;

use asyn_rs::port_handle::PortHandle;

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;

use super::channel::{ndarray_channel, BlockingProcessFn, NDArrayOutput, NDArrayReceiver, NDArraySender};
use super::params::PluginBaseParams;

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
    Int32(usize, i32),
    Float64(usize, f64),
}

/// Result of processing one array: output arrays + param updates to write back.
pub struct ProcessResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
}

impl ProcessResult {
    /// Convenience: sink plugin with only param updates, no output arrays.
    pub fn sink(param_updates: Vec<ParamUpdate>) -> Self {
        Self { output_arrays: vec![], param_updates }
    }

    /// Convenience: passthrough/transform plugin with output arrays but no param updates.
    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self { output_arrays, param_updates: vec![] }
    }

    /// Convenience: no outputs, no param updates.
    pub fn empty() -> Self {
        Self { output_arrays: vec![], param_updates: vec![] }
    }
}

/// Pure processing logic. No threading concerns.
pub trait NDPluginProcess: Send + 'static {
    /// Process one array. Return output arrays and param updates.
    fn process_array(&mut self, array: &NDArray, pool: &NDArrayPool) -> ProcessResult;

    /// Plugin type name for PLUGIN_TYPE param.
    fn plugin_type(&self) -> &str;

    /// Register plugin-specific params on the base. Called once during construction.
    fn register_params(&mut self, _base: &mut PortDriverBase) -> Result<(), asyn_rs::error::AsynError> {
        Ok(())
    }

    /// Called when a param changes. Reason is the param index.
    fn on_param_change(&mut self, _reason: usize, _params: &PluginParamSnapshot) {}
}

/// Read-only snapshot of param values available to the processing thread.
pub struct PluginParamSnapshot {
    pub enable_callbacks: bool,
    /// The param reason that changed.
    pub reason: usize,
    /// The new value.
    pub value: ParamChangeValue,
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
}

impl<P: NDPluginProcess> SharedProcessorInner<P> {
    fn process_and_publish(&mut self, array: &NDArray) {
        let t0 = std::time::Instant::now();
        let result = self.processor.process_array(array, &self.pool);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Publish output arrays to downstream plugins
        let output = self.output.lock();
        for out in &result.output_arrays {
            output.publish(out.clone());
        }
        drop(output);

        // Update base NDArrayDriver params from array metadata
        self.array_counter += 1;
        let info = array.info();
        let color_mode = if array.dims.len() <= 2 { 0 } else { 2 };
        self.port_handle.write_int32_no_wait(self.ndarray_params.array_counter, 0, self.array_counter);
        self.port_handle.write_int32_no_wait(self.ndarray_params.unique_id, 0, array.unique_id);
        self.port_handle.write_int32_no_wait(self.ndarray_params.n_dimensions, 0, array.dims.len() as i32);
        self.port_handle.write_int32_no_wait(self.ndarray_params.array_size_x, 0, info.x_size as i32);
        self.port_handle.write_int32_no_wait(self.ndarray_params.array_size_y, 0, info.y_size as i32);
        self.port_handle.write_int32_no_wait(self.ndarray_params.data_type, 0, array.data.data_type() as i32);
        self.port_handle.write_int32_no_wait(self.ndarray_params.color_mode, 0, color_mode);

        let ts_f64 = array.timestamp.as_f64();
        self.port_handle.write_float64_no_wait(self.ndarray_params.timestamp_rbv, 0, ts_f64);
        self.port_handle.write_int32_no_wait(self.ndarray_params.epics_ts_sec, 0, array.timestamp.sec as i32);
        self.port_handle.write_int32_no_wait(self.ndarray_params.epics_ts_nsec, 0, array.timestamp.nsec as i32);

        self.port_handle.write_float64_no_wait(self.plugin_params.execution_time, 0, elapsed_ms);

        for update in &result.param_updates {
            match update {
                ParamUpdate::Int32(reason, value) => {
                    self.port_handle.write_int32_no_wait(*reason, 0, *value);
                }
                ParamUpdate::Float64(reason, value) => {
                    self.port_handle.write_float64_no_wait(*reason, 0, *value);
                }
            }
        }

        let _ = self.port_handle.call_param_callbacks_blocking(0);
    }
}

/// Type-erased handle for blocking mode: allows NDArraySender to call
/// process_and_publish without knowing the concrete processor type.
struct BlockingProcessorHandle<P: NDPluginProcess> {
    inner: Arc<parking_lot::Mutex<SharedProcessorInner<P>>>,
}

impl<P: NDPluginProcess> BlockingProcessFn for BlockingProcessorHandle<P> {
    fn process_and_publish(&self, array: &NDArray) {
        self.inner.lock().process_and_publish(array);
    }
}

/// PortDriver implementation for a plugin's control plane.
#[allow(dead_code)]
pub struct PluginPortDriver {
    base: PortDriverBase,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    param_change_tx: tokio::sync::mpsc::Sender<(usize, ParamChangeValue)>,
}

impl PluginPortDriver {
    fn new<P: NDPluginProcess>(
        port_name: &str,
        plugin_type_name: &str,
        queue_size: usize,
        ndarray_port: &str,
        param_change_tx: tokio::sync::mpsc::Sender<(usize, ParamChangeValue)>,
        processor: &mut P,
    ) -> AsynResult<Self> {
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                can_block: true,
                ..Default::default()
            },
        );

        let ndarray_params = NDArrayDriverParams::create(&mut base)?;
        let plugin_params = PluginBaseParams::create(&mut base)?;

        // Set defaults (EnableCallbacks=1: Enable by default)
        base.set_int32_param(plugin_params.enable_callbacks, 0, 1)?;
        base.set_int32_param(plugin_params.blocking_callbacks, 0, 0)?;
        base.set_int32_param(plugin_params.queue_size, 0, queue_size as i32)?;
        base.set_int32_param(plugin_params.dropped_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.queue_use, 0, 0)?;
        base.set_string_param(plugin_params.plugin_type, 0, plugin_type_name.into())?;
        base.set_int32_param(ndarray_params.array_callbacks, 0, 1)?;

        // Set plugin identity params
        base.set_string_param(ndarray_params.port_name_self, 0, port_name.into())?;
        if !ndarray_port.is_empty() {
            base.set_string_param(plugin_params.nd_array_port, 0, ndarray_port.into())?;
        }

        // Let the processor register its plugin-specific params
        processor.register_params(&mut base)?;

        Ok(Self {
            base,
            ndarray_params,
            plugin_params,
            param_change_tx,
        })
    }
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
        self.base.set_int32_param(reason, 0, value)?;
        self.base.call_param_callbacks(0)?;
        let _ = self.param_change_tx.try_send((reason, ParamChangeValue::Int32(value)));
        Ok(())
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.base.set_float64_param(reason, 0, value)?;
        self.base.call_param_callbacks(0)?;
        let _ = self.param_change_tx.try_send((reason, ParamChangeValue::Float64(value)));
        Ok(())
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        let reason = user.reason;
        let s = String::from_utf8_lossy(data).into_owned();
        self.base.set_string_param(reason, 0, s.clone())?;
        self.base.call_param_callbacks(0)?;
        let _ = self.param_change_tx.try_send((reason, ParamChangeValue::Octet(s)));
        Ok(())
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
    mut processor: P,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    // Param change channel (control plane -> data plane)
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<(usize, ParamChangeValue)>(64);

    // Capture plugin type before mutable borrow
    let plugin_type_name = processor.plugin_type().to_string();

    // Create the port driver for control plane
    let driver = PluginPortDriver::new(port_name, &plugin_type_name, queue_size, ndarray_port, param_tx, &mut processor)
        .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let blocking_callbacks_reason = driver.plugin_params.blocking_callbacks;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;

    // Create port runtime (actor thread for param I/O)
    let (port_runtime, _actor_jh) =
        create_port_runtime(driver, RuntimeConfig::default());

    // Clone port handle for the data thread to write params back
    let port_handle = port_runtime.port_handle().clone();

    // Array channel (data plane)
    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    // Shared mode flags
    let enabled = Arc::new(AtomicBool::new(true));
    let blocking_mode = Arc::new(AtomicBool::new(false));

    // Shared processor (accessible from both data thread and caller thread)
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
    }));

    // Type-erased handle for blocking mode
    let bp: Arc<dyn BlockingProcessFn> = Arc::new(BlockingProcessorHandle {
        inner: shared.clone(),
    });

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();
    let array_sender = array_sender.with_blocking_support(enabled, blocking_mode, bp);

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
                data_enabled,
                data_blocking,
                nd_array_port_reason,
                sender_port_name,
                initial_upstream,
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
    mut param_rx: tokio::sync::mpsc::Receiver<(usize, ParamChangeValue)>,
    enable_callbacks_reason: usize,
    blocking_callbacks_reason: usize,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    nd_array_port_reason: usize,
    sender_port_name: String,
    initial_upstream: String,
) {
    let mut current_upstream = initial_upstream;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        loop {
            tokio::select! {
                msg = array_rx.recv_msg() => {
                    match msg {
                        Some(msg) => {
                            // In blocking mode, arrays are processed inline by the caller.
                            // Skip processing here to avoid double-processing.
                            if !blocking_mode.load(Ordering::Acquire) {
                                shared.lock().process_and_publish(&msg.array);
                            }
                            // msg dropped here → completion signaled (if tracked)
                        }
                        None => break,
                    }
                }
                param = param_rx.recv() => {
                    match param {
                        Some((reason, value)) => {
                            if reason == enable_callbacks_reason {
                                enabled.store(value.as_i32() != 0, Ordering::Release);
                            }
                            if reason == blocking_callbacks_reason {
                                blocking_mode.store(value.as_i32() != 0, Ordering::Release);
                            }
                            // Handle NDArrayPort rewiring
                            if reason == nd_array_port_reason {
                                if let Some(new_port) = value.as_string() {
                                    let old = std::mem::replace(&mut current_upstream, new_port.to_string());
                                    if let Err(e) = super::wiring::rewire_by_name(&sender_port_name, &old, new_port) {
                                        eprintln!("NDArrayPort rewire failed: {e}");
                                        // Revert current_upstream on failure
                                        current_upstream = old;
                                    }
                                }
                            }
                            let snapshot = PluginParamSnapshot {
                                enable_callbacks: enabled.load(Ordering::Acquire),
                                reason,
                                value,
                            };
                            shared.lock().processor.on_param_change(reason, &snapshot);
                        }
                        None => break,
                    }
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
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<(usize, ParamChangeValue)>(64);

    let plugin_type_name = processor.plugin_type().to_string();
    let driver = PluginPortDriver::new(port_name, &plugin_type_name, queue_size, ndarray_port, param_tx, &mut processor)
        .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let blocking_callbacks_reason = driver.plugin_params.blocking_callbacks;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;

    let (port_runtime, _actor_jh) =
        create_port_runtime(driver, RuntimeConfig::default());

    let port_handle = port_runtime.port_handle().clone();

    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    let enabled = Arc::new(AtomicBool::new(true));
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
    }));

    let bp: Arc<dyn BlockingProcessFn> = Arc::new(BlockingProcessorHandle {
        inner: shared.clone(),
    });

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();
    let array_sender = array_sender.with_blocking_support(enabled, blocking_mode, bp);

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
                data_enabled,
                data_blocking,
                nd_array_port_reason,
                sender_port_name,
                initial_upstream,
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
        );

        // Send an array
        handle.array_sender().send(make_test_array(42));

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
        );

        // Send arrays - they should be consumed silently
        handle.array_sender().send(make_test_array(1));
        handle.array_sender().send(make_test_array(2));

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
    fn test_dropped_count_when_queue_full() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        // Very slow processor
        struct SlowProcessor;
        impl NDPluginProcess for SlowProcessor {
            fn process_array(
                &mut self,
                _array: &NDArray,
                _pool: &NDArrayPool,
            ) -> ProcessResult {
                std::thread::sleep(std::time::Duration::from_millis(100));
                ProcessResult::empty()
            }
            fn plugin_type(&self) -> &str {
                "Slow"
            }
        }

        let (handle, _data_jh) = create_plugin_runtime(
            "DROP_TEST",
            SlowProcessor,
            pool,
            1,
            "",
        );

        // Fill the queue and overflow
        for i in 0..10 {
            handle.array_sender().send(make_test_array(i));
        }

        // Some should have been dropped
        assert!(handle.array_sender().dropped_count() > 0);
    }

    #[test]
    fn test_blocking_callbacks_basic() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "BLOCK_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
        );

        // Enable blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // In blocking mode, send() processes inline and returns synchronously
        handle.array_sender().send(make_test_array(42));

        // Array should already be in the downstream channel
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
        );

        // Start in blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        handle.array_sender().send(make_test_array(1));
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
        handle.array_sender().send(make_test_array(2));
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
        );

        // Disable callbacks
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Send array — should be silently dropped by sender
        handle.array_sender().send(make_test_array(99));

        // Verify nothing received (with timeout)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                downstream_rx.recv(),
            )
            .await
        });
        assert!(
            result.is_err(),
            "should not receive array when callbacks disabled"
        );
    }

    #[test]
    fn test_blocking_downstream_receives() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));

        let (ds1, mut rx1) = ndarray_channel("DS1", 10);
        let (ds2, mut rx2) = ndarray_channel("DS2", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds1);
        output.add(ds2);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            "BLOCK_DS_TEST",
            PassthroughProcessor,
            pool,
            10,
            output,
            "",
        );

        // Enable blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        handle.array_sender().send(make_test_array(77));

        // Both downstream receivers should have the array
        let r1 = rx1.blocking_recv().unwrap();
        let r2 = rx2.blocking_recv().unwrap();
        assert_eq!(r1.unique_id, 77);
        assert_eq!(r2.unique_id, 77);
    }

    #[test]
    fn test_blocking_param_updates() {
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
        );

        // Enable blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Send array in blocking mode
        handle.array_sender().send(make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Write enable_callbacks while in blocking mode — should not crash
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Still works after param update
        handle.array_sender().send(make_test_array(2));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 2);
    }
}
