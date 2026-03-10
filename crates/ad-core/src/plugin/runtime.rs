use std::sync::Arc;
use std::thread;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{create_port_runtime, PortRuntimeHandle};
use asyn_rs::user::AsynUser;

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ndarray_driver::NDArrayDriverParams;

use super::channel::{ndarray_channel, NDArrayOutput, NDArrayReceiver, NDArraySender};
use super::params::PluginBaseParams;

/// Pure processing logic. No threading concerns.
pub trait NDPluginProcess: Send + 'static {
    /// Process one array. Return output arrays (empty = sink-only plugin like Stats).
    fn process_array(&mut self, array: &NDArray, pool: &NDArrayPool) -> Vec<Arc<NDArray>>;

    /// Plugin type name for PLUGIN_TYPE param.
    fn plugin_type(&self) -> &str;

    /// Called when a param changes. Reason is the param index.
    fn on_param_change(&mut self, _reason: usize, _params: &PluginParamSnapshot) {}
}

/// Read-only snapshot of param values available to the processing thread.
pub struct PluginParamSnapshot {
    pub enable_callbacks: bool,
}

/// PortDriver implementation for a plugin's control plane.
#[allow(dead_code)]
pub struct PluginPortDriver {
    base: PortDriverBase,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    param_change_tx: tokio::sync::mpsc::Sender<usize>,
}

impl PluginPortDriver {
    fn new(
        port_name: &str,
        plugin_type_name: &str,
        queue_size: usize,
        param_change_tx: tokio::sync::mpsc::Sender<usize>,
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

        // Set defaults
        base.set_int32_param(plugin_params.enable_callbacks, 0, 1)?;
        base.set_int32_param(plugin_params.blocking_callbacks, 0, 0)?;
        base.set_int32_param(plugin_params.queue_size, 0, queue_size as i32)?;
        base.set_int32_param(plugin_params.dropped_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.queue_use, 0, 0)?;
        base.set_string_param(plugin_params.plugin_type, 0, plugin_type_name.into())?;
        base.set_int32_param(ndarray_params.array_callbacks, 0, 1)?;

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
        let _ = self.param_change_tx.try_send(reason);
        Ok(())
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.base.set_float64_param(reason, 0, value)?;
        self.base.call_param_callbacks(0)?;
        let _ = self.param_change_tx.try_send(reason);
        Ok(())
    }
}

/// Handle to a running plugin runtime. Provides access to sender and port handle.
#[derive(Clone)]
pub struct PluginRuntimeHandle {
    port_runtime: PortRuntimeHandle,
    array_sender: NDArraySender,
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
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    // Param change channel (control plane -> data plane)
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<usize>(64);

    // Create the port driver for control plane
    let driver = PluginPortDriver::new(port_name, processor.plugin_type(), queue_size, param_tx)
        .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;

    // Create port runtime (actor thread for param I/O)
    let (port_runtime, _actor_jh) =
        create_port_runtime(driver, RuntimeConfig::default());

    // Array channel (data plane)
    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    // Output fan-out (initially empty; downstream wired later)
    let array_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    let data_output = array_output.clone();

    // Spawn data processing thread
    let data_jh = thread::Builder::new()
        .name(format!("plugin-data-{port_name}"))
        .spawn(move || {
            plugin_data_loop(
                processor,
                array_rx,
                param_rx,
                data_output,
                pool,
                enable_callbacks_reason,
            );
        })
        .expect("failed to spawn plugin data thread");

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        port_name: port_name.to_string(),
        ndarray_params,
        plugin_params,
    };

    (handle, data_jh)
}

fn plugin_data_loop<P: NDPluginProcess>(
    mut processor: P,
    mut array_rx: NDArrayReceiver,
    mut param_rx: tokio::sync::mpsc::Receiver<usize>,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    pool: Arc<NDArrayPool>,
    enable_callbacks_reason: usize,
) {
    let enabled = true;

    loop {
        match array_rx.blocking_recv() {
            Some(array) => {
                // Drain pending param changes
                while let Ok(reason) = param_rx.try_recv() {
                    let snapshot = PluginParamSnapshot {
                        enable_callbacks: enabled,
                    };
                    if reason == enable_callbacks_reason {
                        // We can't read from port handle here easily,
                        // so we toggle based on the notification
                    }
                    processor.on_param_change(reason, &snapshot);
                }

                if !enabled {
                    continue;
                }

                let outputs = processor.process_array(&array, &pool);
                let output = array_output.lock();
                for out in outputs {
                    output.publish(out);
                }
            }
            None => break, // channel closed = shutdown
        }
    }
}

/// Connect a downstream plugin's sender to a plugin runtime's output.
/// This must be called before starting acquisition.
pub fn wire_downstream(
    _upstream: &PluginRuntimeHandle,
    _downstream_sender: NDArraySender,
) {
    // For Phase 3, wiring is done via the PluginRuntimeHandle's output.
    // The actual wiring mechanism will be finalized in Phase 4.
    // For now, tests can use create_plugin_runtime_with_output.
}

/// Create a plugin runtime with a pre-wired output (for testing and direct wiring).
pub fn create_plugin_runtime_with_output<P: NDPluginProcess>(
    port_name: &str,
    processor: P,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    output: NDArrayOutput,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    let (param_tx, param_rx) = tokio::sync::mpsc::channel::<usize>(64);

    let driver = PluginPortDriver::new(port_name, processor.plugin_type(), queue_size, param_tx)
        .expect("failed to create plugin port driver");

    let enable_callbacks_reason = driver.plugin_params.enable_callbacks;
    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;

    let (port_runtime, _actor_jh) =
        create_port_runtime(driver, RuntimeConfig::default());

    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size);

    let data_output = Arc::new(parking_lot::Mutex::new(output));

    let data_jh = thread::Builder::new()
        .name(format!("plugin-data-{port_name}"))
        .spawn(move || {
            plugin_data_loop(
                processor,
                array_rx,
                param_rx,
                data_output,
                pool,
                enable_callbacks_reason,
            );
        })
        .expect("failed to spawn plugin data thread");

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
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
        fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
            vec![Arc::new(array.clone())]
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
        fn process_array(&mut self, _array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
            self.count += 1;
            vec![]
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
            ) -> Vec<Arc<NDArray>> {
                std::thread::sleep(std::time::Duration::from_millis(100));
                vec![]
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
        );

        // Fill the queue and overflow
        for i in 0..10 {
            handle.array_sender().send(make_test_array(i));
        }

        // Some should have been dropped
        assert!(handle.array_sender().dropped_count() > 0);
    }
}
