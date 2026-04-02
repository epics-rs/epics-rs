use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::user::AsynUser;

use ad_core_rs::driver::{ADDriver, ADDriverBase, ImageMode};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

use crate::params::SimDetectorParams;
use crate::task::{AcqCommand, AcquisitionContext, start_acquisition_task};
use crate::types::DirtyFlags;

pub struct SimDetector {
    pub ad: ADDriverBase,
    pub sim_params: SimDetectorParams,
    pub dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    acq_tx: std::sync::mpsc::Sender<AcqCommand>,
}

impl SimDetector {
    pub fn new(
        port_name: &str,
        max_size_x: i32,
        max_size_y: i32,
        max_memory: usize,
        acq_tx: std::sync::mpsc::Sender<AcqCommand>,
        dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    ) -> AsynResult<Self> {
        let mut ad = ADDriverBase::new(port_name, max_size_x, max_size_y, max_memory)?;
        let sim_params = SimDetectorParams::create(&mut ad.port_base)?;

        // Set default values matching C++ constructor
        let base = &mut ad.port_base;
        base.set_string_param(ad.params.base.manufacturer, 0, "Simulated detector".into())?;
        base.set_string_param(ad.params.base.model, 0, "Basic simulator".into())?;
        base.set_string_param(ad.params.base.serial_number, 0, "No serial number".into())?;
        base.set_string_param(ad.params.base.firmware_version, 0, "No firmware".into())?;
        base.set_string_param(
            ad.params.base.sdk_version,
            0,
            env!("CARGO_PKG_VERSION").into(),
        )?;

        base.set_int32_param(ad.params.min_x, 0, 0)?;
        base.set_int32_param(ad.params.min_y, 0, 0)?;
        base.set_float64_param(ad.params.acquire_time, 0, 0.001)?;
        base.set_float64_param(ad.params.acquire_period, 0, 0.005)?;
        base.set_int32_param(ad.params.image_mode, 0, ImageMode::Continuous as i32)?;
        base.set_int32_param(ad.params.num_images, 0, 100)?;

        base.set_float64_param(sim_params.gain, 0, 1.0)?;
        base.set_float64_param(sim_params.gain_x, 0, 1.0)?;
        base.set_float64_param(sim_params.gain_y, 0, 1.0)?;
        base.set_float64_param(sim_params.gain_red, 0, 1.0)?;
        base.set_float64_param(sim_params.gain_green, 0, 1.0)?;
        base.set_float64_param(sim_params.gain_blue, 0, 1.0)?;
        base.set_float64_param(sim_params.offset, 0, 0.0)?;
        base.set_float64_param(sim_params.noise, 0, 0.0)?;
        base.set_int32_param(sim_params.sim_mode, 0, 0)?;
        base.set_int32_param(sim_params.peak_start_x, 0, 1)?;
        base.set_int32_param(sim_params.peak_start_y, 0, 1)?;
        base.set_int32_param(sim_params.peak_width_x, 0, 10)?;
        base.set_int32_param(sim_params.peak_width_y, 0, 20)?;
        base.set_int32_param(sim_params.peak_num_x, 0, 1)?;
        base.set_int32_param(sim_params.peak_num_y, 0, 1)?;
        base.set_int32_param(sim_params.peak_step_x, 0, 1)?;
        base.set_int32_param(sim_params.peak_step_y, 0, 1)?;
        base.set_float64_param(sim_params.peak_height_variation, 0, 0.0)?;
        base.set_int32_param(sim_params.x_sine_operation, 0, 0)?;
        base.set_int32_param(sim_params.y_sine_operation, 0, 0)?;
        base.set_float64_param(sim_params.x_sine1_amplitude, 0, 0.0)?;
        base.set_float64_param(sim_params.x_sine1_frequency, 0, 0.0)?;
        base.set_float64_param(sim_params.x_sine1_phase, 0, 0.0)?;
        base.set_float64_param(sim_params.x_sine2_amplitude, 0, 0.0)?;
        base.set_float64_param(sim_params.x_sine2_frequency, 0, 0.0)?;
        base.set_float64_param(sim_params.x_sine2_phase, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine1_amplitude, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine1_frequency, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine1_phase, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine2_amplitude, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine2_frequency, 0, 0.0)?;
        base.set_float64_param(sim_params.y_sine2_phase, 0, 0.0)?;

        // Reset image flag - triggers initial buffer allocation
        base.set_int32_param(sim_params.reset_image, 0, 1)?;

        Ok(Self {
            ad,
            sim_params,
            dirty,
            acq_tx,
        })
    }

    fn set_dirty_for_int32(&self, reason: usize) {
        let mut dirty = self.dirty.lock();
        if reason == self.ad.params.base.data_type || reason == self.ad.params.base.color_mode {
            dirty.reallocate_buffers = true;
            dirty.rebuild_background = true;
            dirty.reset_ramp = true;
            dirty.reset_peak_cache = true;
            dirty.reset_sine_state = true;
        } else if reason == self.sim_params.sim_mode {
            dirty.reset_ramp = true;
            dirty.reset_peak_cache = true;
            dirty.reset_sine_state = true;
            dirty.rebuild_background = true;
        } else if reason == self.sim_params.peak_start_x
            || reason == self.sim_params.peak_start_y
            || reason == self.sim_params.peak_width_x
            || reason == self.sim_params.peak_width_y
            || reason == self.sim_params.peak_num_x
            || reason == self.sim_params.peak_num_y
            || reason == self.sim_params.peak_step_x
            || reason == self.sim_params.peak_step_y
        {
            dirty.reset_peak_cache = true;
        } else if reason == self.sim_params.x_sine_operation
            || reason == self.sim_params.y_sine_operation
        {
            dirty.reset_sine_state = true;
        }
    }

    fn set_dirty_for_float64(&self, reason: usize) {
        let mut dirty = self.dirty.lock();
        if reason == self.sim_params.gain {
            dirty.reset_ramp = true;
            dirty.reset_peak_cache = true;
        } else if reason == self.sim_params.gain_x || reason == self.sim_params.gain_y {
            dirty.reset_ramp = true;
        } else if reason == self.sim_params.gain_red
            || reason == self.sim_params.gain_green
            || reason == self.sim_params.gain_blue
        {
            dirty.reset_ramp = true;
        } else if reason == self.sim_params.offset || reason == self.sim_params.noise {
            dirty.rebuild_background = true;
        } else if reason == self.sim_params.peak_height_variation {
            dirty.reset_peak_cache = true;
        } else if reason == self.sim_params.x_sine1_amplitude
            || reason == self.sim_params.x_sine1_frequency
            || reason == self.sim_params.x_sine1_phase
            || reason == self.sim_params.x_sine2_amplitude
            || reason == self.sim_params.x_sine2_frequency
            || reason == self.sim_params.x_sine2_phase
            || reason == self.sim_params.y_sine1_amplitude
            || reason == self.sim_params.y_sine1_frequency
            || reason == self.sim_params.y_sine1_phase
            || reason == self.sim_params.y_sine2_amplitude
            || reason == self.sim_params.y_sine2_frequency
            || reason == self.sim_params.y_sine2_phase
        {
            dirty.reset_sine_state = true;
        }
    }
}

impl PortDriver for SimDetector {
    fn base(&self) -> &PortDriverBase {
        &self.ad.port_base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.ad.port_base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        let acquire_idx = self.ad.params.acquire;
        let status_msg_idx = self.ad.params.status_message;

        if reason == acquire_idx {
            let acquiring = self
                .ad
                .port_base
                .get_int32_param(acquire_idx, 0)
                .unwrap_or(0);
            if value != 0 && acquiring == 0 {
                self.ad
                    .port_base
                    .set_string_param(status_msg_idx, 0, "Acquiring data".into())?;
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
                let _ = self.acq_tx.send(AcqCommand::Start);
            } else if value == 0 && acquiring != 0 {
                self.ad.port_base.set_string_param(
                    status_msg_idx,
                    0,
                    "Acquisition stopped".into(),
                )?;
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
                let _ = self.acq_tx.send(AcqCommand::Stop);
            } else {
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
            }
        } else {
            self.ad
                .port_base
                .params
                .set_int32(reason, user.addr, value)?;
            self.set_dirty_for_int32(reason);
            self.ad.port_base.call_param_callback(0, reason)?;
            return Ok(());
        }

        self.ad.port_base.call_param_callbacks(0)?;
        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.ad
            .port_base
            .params
            .set_float64(reason, user.addr, value)?;
        self.set_dirty_for_float64(reason);
        self.ad.port_base.call_param_callback(0, reason)?;
        Ok(())
    }
}

impl ADDriver for SimDetector {
    fn ad_base(&self) -> &ADDriverBase {
        &self.ad
    }

    fn ad_base_mut(&mut self) -> &mut ADDriverBase {
        &mut self.ad
    }
}

/// Handle to a running SimDetector runtime.
pub struct SimDetectorRuntime {
    pub runtime_handle: PortRuntimeHandle,
    pub ad_params: ADBaseParams,
    pub sim_params: SimDetectorParams,
    pool: Arc<NDArrayPool>,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    queued_counter: Arc<QueuedArrayCounter>,
    #[allow(dead_code)]
    task_handle: Option<std::thread::JoinHandle<()>>,
}

impl SimDetectorRuntime {
    pub fn port_handle(&self) -> &PortHandle {
        self.runtime_handle.port_handle()
    }

    pub fn pool(&self) -> &Arc<NDArrayPool> {
        &self.pool
    }

    /// The shared array output (for building a DriverContext).
    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_output
    }

    /// Connect a downstream plugin's sender to this detector's output fan-out.
    pub fn connect_downstream(&self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.lock().add(sender);
    }
}

/// Create a SimDetector with actor-based runtime and acquisition task.
///
/// The `array_output` is wrapped in `Arc<Mutex>` and shared between the
/// acquisition task and the runtime, allowing downstream plugins to be
/// connected after creation via `connect_downstream()`.
pub fn create_sim_detector(
    port_name: &str,
    max_size_x: i32,
    max_size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
) -> AsynResult<SimDetectorRuntime> {
    let (acq_tx, acq_rx) = std::sync::mpsc::channel();
    let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
    dirty.lock().set_all();

    let det = SimDetector::new(
        port_name,
        max_size_x,
        max_size_y,
        max_memory,
        acq_tx,
        dirty.clone(),
    )?;

    // Capture param indices before driver is moved into PortActor
    let ad_params = det.ad.params;
    let sim_params = det.sim_params;
    let pool = det.ad.pool.clone();

    let (runtime_handle, _actor_jh) = create_port_runtime(det, RuntimeConfig::default());

    let shared_output = Arc::new(parking_lot::Mutex::new(array_output));
    let queued_counter = Arc::new(QueuedArrayCounter::new());

    let task_handle = start_acquisition_task(AcquisitionContext {
        acq_rx,
        port_handle: runtime_handle.port_handle().clone(),
        array_output: shared_output.clone(),
        dirty,
        ad: ad_params,
        sim: sim_params,
        queued_counter: queued_counter.clone(),
    });

    Ok(SimDetectorRuntime {
        runtime_handle,
        ad_params,
        sim_params,
        pool,
        array_output: shared_output,
        queued_counter,
        task_handle: Some(task_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::plugin::channel::NDArrayOutput;

    #[test]
    fn test_new_default_values() {
        let rt = create_sim_detector("SIM1", 256, 256, 10_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        assert_eq!(
            handle
                .read_int32_blocking(rt.ad_params.max_size_x, 0)
                .unwrap(),
            256
        );
        assert_eq!(
            handle
                .read_int32_blocking(rt.ad_params.max_size_y, 0)
                .unwrap(),
            256
        );
        assert_eq!(
            handle
                .read_float64_blocking(rt.sim_params.gain_x, 0)
                .unwrap(),
            1.0
        );
        assert_eq!(
            handle
                .read_float64_blocking(rt.sim_params.gain_y, 0)
                .unwrap(),
            1.0
        );
        assert_eq!(
            handle
                .read_int32_blocking(rt.sim_params.peak_width_x, 0)
                .unwrap(),
            10
        );
        assert_eq!(
            handle
                .read_int32_blocking(rt.sim_params.peak_width_y, 0)
                .unwrap(),
            20
        );
        assert!(
            (handle
                .read_float64_blocking(rt.ad_params.acquire_time, 0)
                .unwrap()
                - 0.001)
                .abs()
                < 1e-10
        );
    }

    #[test]
    fn test_dirty_flags_on_data_type_change() {
        let rt = create_sim_detector("SIM2", 64, 64, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        // Write data_type via PortHandle (triggers write_int32 -> set_dirty)
        handle
            .write_int32_blocking(rt.ad_params.base.data_type, 0, 3)
            .unwrap();
        // Can't directly check dirty flags from outside since driver is owned by actor,
        // but the write succeeds without error.
    }

    #[test]
    fn test_dirty_flags_on_gain_change() {
        let rt = create_sim_detector("SIM3", 64, 64, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        handle
            .write_float64_blocking(rt.sim_params.gain, 0, 2.0)
            .unwrap();
        // Verify write took effect
        assert!((handle.read_float64_blocking(rt.sim_params.gain, 0).unwrap() - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_dirty_flags_on_offset_change() {
        let rt = create_sim_detector("SIM4", 64, 64, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        handle
            .write_float64_blocking(rt.sim_params.offset, 0, 5.0)
            .unwrap();
        assert!(
            (handle
                .read_float64_blocking(rt.sim_params.offset, 0)
                .unwrap()
                - 5.0)
                .abs()
                < 1e-10
        );
    }

    #[test]
    fn test_dirty_flags_on_sine_param_change() {
        let rt = create_sim_detector("SIM5", 64, 64, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        handle
            .write_float64_blocking(rt.sim_params.x_sine1_amplitude, 0, 100.0)
            .unwrap();
        assert!(
            (handle
                .read_float64_blocking(rt.sim_params.x_sine1_amplitude, 0)
                .unwrap()
                - 100.0)
                .abs()
                < 1e-10
        );
    }

    #[test]
    fn test_dirty_flags_on_mode_change() {
        let rt = create_sim_detector("SIM6", 64, 64, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        handle
            .write_int32_blocking(rt.sim_params.sim_mode, 0, 2)
            .unwrap();
        assert_eq!(
            handle
                .read_int32_blocking(rt.sim_params.sim_mode, 0)
                .unwrap(),
            2
        );
    }
}
