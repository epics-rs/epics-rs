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

use crate::physics::MovingDotImageConfig;

use super::params::MovingDotParams;
use super::task::{AcqCommand, AcquisitionContext, start_acquisition_task};
use super::types::DirtyFlags;

/// MovingDot area detector driver.
pub struct MovingDotDetector {
    pub ad: ADDriverBase,
    pub dot_params: MovingDotParams,
    dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    acq_tx: std::sync::mpsc::Sender<AcqCommand>,
}

impl MovingDotDetector {
    pub fn new(
        port_name: &str,
        max_size_x: i32,
        max_size_y: i32,
        max_memory: usize,
        acq_tx: std::sync::mpsc::Sender<AcqCommand>,
        dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    ) -> AsynResult<Self> {
        let mut ad = ADDriverBase::new(port_name, max_size_x, max_size_y, max_memory)?;
        let dot_params = MovingDotParams::create(&mut ad.port_base)?;

        // Set defaults
        let base = &mut ad.port_base;
        base.set_string_param(ad.params.base.manufacturer, 0, "Mini Beamline".into())?;
        base.set_string_param(ad.params.base.model, 0, "Moving Dot".into())?;

        base.set_float64_param(ad.params.acquire_time, 0, 0.1)?;
        base.set_float64_param(ad.params.acquire_period, 0, 0.5)?;
        base.set_int32_param(ad.params.image_mode, 0, ImageMode::Continuous as i32)?;
        base.set_int32_param(ad.params.num_images, 0, 100)?;

        base.set_int32_param(ad.params.base.data_type, 0, 3)?; // UInt16
        base.set_int32_param(ad.params.bin_x, 0, 1)?;
        base.set_int32_param(ad.params.bin_y, 0, 1)?;

        base.set_float64_param(dot_params.motor_x_pos, 0, 0.0)?;
        base.set_float64_param(dot_params.motor_y_pos, 0, 0.0)?;
        base.set_float64_param(dot_params.beam_current, 0, 500.0)?;
        base.set_int32_param(dot_params.shutter_open, 0, 1)?;

        // Slit aperture: 0 means no limit (full image)
        base.set_float64_param(dot_params.slit_left, 0, 0.0)?;
        base.set_float64_param(dot_params.slit_right, 0, 640.0)?;
        base.set_float64_param(dot_params.slit_top, 0, 0.0)?;
        base.set_float64_param(dot_params.slit_bottom, 0, 480.0)?;

        dirty.lock().set();

        Ok(Self {
            ad,
            dot_params,
            dirty,
            acq_tx,
        })
    }
}

impl PortDriver for MovingDotDetector {
    fn base(&self) -> &PortDriverBase {
        &self.ad.port_base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.ad.port_base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        let acquire_idx = self.ad.params.acquire;

        if reason == acquire_idx {
            let acquiring = self
                .ad
                .port_base
                .get_int32_param(acquire_idx, 0)
                .unwrap_or(0);
            if value != 0 && acquiring == 0 {
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
                // Flush Acquire state immediately so the record reflects the change
                self.ad.port_base.call_param_callbacks(0)?;
                let _ = self.acq_tx.send(AcqCommand::Start);
            } else if value == 0 && acquiring != 0 {
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
            if reason == self.dot_params.shutter_open
                || reason == self.ad.params.bin_x
                || reason == self.ad.params.bin_y
            {
                self.dirty.lock().set();
            }
            self.ad.port_base.call_param_callback(0, reason)?;
        }

        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.ad
            .port_base
            .params
            .set_float64(reason, user.addr, value)?;

        if reason == self.dot_params.motor_x_pos
            || reason == self.dot_params.motor_y_pos
            || reason == self.dot_params.beam_current
            || reason == self.ad.params.acquire_time
            || reason == self.ad.params.acquire_period
            || reason == self.dot_params.slit_left
            || reason == self.dot_params.slit_right
            || reason == self.dot_params.slit_top
            || reason == self.dot_params.slit_bottom
        {
            self.dirty.lock().set();
        }

        self.ad.port_base.call_param_callback(0, reason)?;
        Ok(())
    }
}

impl ADDriver for MovingDotDetector {
    fn ad_base(&self) -> &ADDriverBase {
        &self.ad
    }

    fn ad_base_mut(&mut self) -> &mut ADDriverBase {
        &mut self.ad
    }
}

/// Handle to a running MovingDot detector runtime.
pub struct MovingDotRuntime {
    pub runtime_handle: PortRuntimeHandle,
    pub ad_params: ADBaseParams,
    pub dot_params: MovingDotParams,
    pool: Arc<NDArrayPool>,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    queued_counter: Arc<QueuedArrayCounter>,
    #[allow(dead_code)]
    task_handle: Option<std::thread::JoinHandle<()>>,
}

impl MovingDotRuntime {
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

    pub fn connect_downstream(&self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.lock().add(sender);
    }
}

/// Create a MovingDot detector with actor-based runtime and acquisition task.
pub fn create_moving_dot(
    port_name: &str,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
) -> AsynResult<MovingDotRuntime> {
    create_moving_dot_with_config(
        port_name,
        size_x,
        size_y,
        max_memory,
        array_output,
        MovingDotImageConfig::default(),
    )
}

/// Create a MovingDot detector with custom image generation config.
pub fn create_moving_dot_with_config(
    port_name: &str,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
    image_config: MovingDotImageConfig,
) -> AsynResult<MovingDotRuntime> {
    let (acq_tx, acq_rx) = std::sync::mpsc::channel();
    let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
    dirty.lock().set();

    let det = MovingDotDetector::new(port_name, size_x, size_y, max_memory, acq_tx, dirty.clone())?;

    let ad_params = det.ad.params;
    let dot_params = det.dot_params;
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
        dot: dot_params,
        image_config,
        queued_counter: queued_counter.clone(),
    });

    Ok(MovingDotRuntime {
        runtime_handle,
        ad_params,
        dot_params,
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
    fn test_create_moving_dot() {
        let rt = create_moving_dot("DOT_TEST", 640, 480, 10_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();
        assert_eq!(
            handle
                .read_int32_blocking(rt.ad_params.max_size_x, 0)
                .unwrap(),
            640
        );
        assert_eq!(
            handle
                .read_int32_blocking(rt.ad_params.max_size_y, 0)
                .unwrap(),
            480
        );
    }

    #[test]
    fn test_single_frame_acquisition() {
        let rt = create_moving_dot("DOT_SINGLE", 64, 48, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();

        handle
            .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.001)
            .unwrap();

        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
        assert_eq!(acquire, 0, "acquire should be 0 after Single mode");
        let counter = handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap();
        assert!(counter >= 1, "should have produced at least 1 frame");
    }
}
