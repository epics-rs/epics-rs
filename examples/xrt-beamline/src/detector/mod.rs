pub mod params;
pub mod task;
pub mod types;

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
use ad_core_rs::plugin::channel::{
    ArrayPublisher, NDArrayOutput, NDArraySender, QueuedArrayCounter,
};
use ad_core_rs::runtime as rt;

use crate::beamline_sim::{MotorPositions, SimConfig};

use params::XrtDetectorParams;
use task::{AcqCommand, AcquisitionContext, start_acquisition_task};
use types::DirtyFlags;

/// XRT beamline detector driver (AreaDetector).
pub struct XrtDetector {
    pub ad: ADDriverBase,
    pub xrt_params: XrtDetectorParams,
    motor_param_set: Vec<usize>,
    dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    acq_tx: rt::CommandSender<AcqCommand>,
}

impl XrtDetector {
    pub fn new(
        port_name: &str,
        size_x: i32,
        size_y: i32,
        max_memory: usize,
        acq_tx: rt::CommandSender<AcqCommand>,
        dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    ) -> AsynResult<Self> {
        let mut ad = ADDriverBase::new(port_name, size_x, size_y, max_memory)?;
        let xrt_params = XrtDetectorParams::create(&mut ad.port_base)?;
        let motor_param_set = xrt_params.motor_params();

        let base = &mut ad.port_base;
        base.set_string_param(ad.params.base.manufacturer, 0, "XRT Beamline")?;
        base.set_string_param(ad.params.base.model, 0, "Ray Tracing Simulator")?;

        base.set_float64_param(ad.params.acquire_time, 0, 0.1)?;
        base.set_float64_param(ad.params.acquire_period, 0, 0.1)?;
        base.set_int32_param(ad.params.image_mode, 0, ImageMode::Continuous as i32)?;
        base.set_int32_param(ad.params.num_images, 0, 100)?;
        base.set_int32_param(ad.params.base.data_type, 0, 3)?; // UInt16

        // Default motor positions
        let defaults = MotorPositions::default();
        base.set_float64_param(xrt_params.und_gap, 0, defaults.und_gap)?;
        base.set_float64_param(xrt_params.und_x, 0, defaults.und_x)?;
        base.set_float64_param(xrt_params.und_z, 0, defaults.und_z)?;
        base.set_float64_param(xrt_params.dcm_theta, 0, defaults.dcm_theta)?;
        base.set_float64_param(xrt_params.dcm_theta2, 0, defaults.dcm_theta2)?;
        base.set_float64_param(xrt_params.dcm_y, 0, defaults.dcm_y)?;
        base.set_float64_param(xrt_params.dcm_chi1, 0, defaults.dcm_chi1)?;
        base.set_float64_param(xrt_params.dcm_chi2, 0, defaults.dcm_chi2)?;
        base.set_float64_param(xrt_params.dcm_z, 0, defaults.dcm_z)?;
        base.set_float64_param(xrt_params.hfm_pitch, 0, defaults.hfm_pitch)?;
        base.set_float64_param(xrt_params.hfm_roll, 0, defaults.hfm_roll)?;
        base.set_float64_param(xrt_params.hfm_yaw, 0, defaults.hfm_yaw)?;
        base.set_float64_param(xrt_params.hfm_x, 0, defaults.hfm_x)?;
        base.set_float64_param(xrt_params.hfm_y, 0, defaults.hfm_y)?;
        base.set_float64_param(xrt_params.hfm_z, 0, defaults.hfm_z)?;
        base.set_float64_param(xrt_params.hfm_r_major, 0, defaults.hfm_r_major)?;
        base.set_float64_param(xrt_params.hfm_r_minor, 0, defaults.hfm_r_minor)?;
        base.set_float64_param(xrt_params.vfm_pitch, 0, defaults.vfm_pitch)?;
        base.set_float64_param(xrt_params.vfm_roll, 0, defaults.vfm_roll)?;
        base.set_float64_param(xrt_params.vfm_yaw, 0, defaults.vfm_yaw)?;
        base.set_float64_param(xrt_params.vfm_x, 0, defaults.vfm_x)?;
        base.set_float64_param(xrt_params.vfm_y, 0, defaults.vfm_y)?;
        base.set_float64_param(xrt_params.vfm_z, 0, defaults.vfm_z)?;
        base.set_float64_param(xrt_params.vfm_r_major, 0, defaults.vfm_r_major)?;
        base.set_float64_param(xrt_params.vfm_r_minor, 0, defaults.vfm_r_minor)?;

        dirty.lock().set();

        Ok(Self {
            ad,
            xrt_params,
            motor_param_set,
            dirty,
            acq_tx,
        })
    }
}

impl PortDriver for XrtDetector {
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
                self.ad
                    .port_base
                    .set_int32_param(self.ad.params.acquire_busy, 0, 1)?;
                self.ad.port_base.set_int32_param(
                    self.ad.params.status,
                    0,
                    ad_core_rs::driver::ADStatus::Acquire as i32,
                )?;
                self.ad.port_base.call_param_callbacks(0)?;
                let _ = self.acq_tx.try_send(AcqCommand::Start);
            } else if value == 0 && acquiring != 0 {
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
                let _ = self.acq_tx.try_send(AcqCommand::Stop);
            } else {
                self.ad.port_base.set_int32_param(acquire_idx, 0, value)?;
            }
        } else {
            self.ad
                .port_base
                .params
                .set_int32(reason, user.addr, value)?;
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

        if self.motor_param_set.contains(&reason) {
            self.dirty.lock().set();
        }

        self.ad.port_base.call_param_callback(0, reason)?;
        Ok(())
    }
}

impl ADDriver for XrtDetector {
    fn ad_base(&self) -> &ADDriverBase {
        &self.ad
    }

    fn ad_base_mut(&mut self) -> &mut ADDriverBase {
        &mut self.ad
    }
}

/// Handle to a running XRT detector runtime.
pub struct XrtDetectorRuntime {
    pub runtime_handle: PortRuntimeHandle,
    pub ad_params: ADBaseParams,
    pub xrt_params: XrtDetectorParams,
    pool: Arc<NDArrayPool>,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    queued_counter: Arc<QueuedArrayCounter>,
    #[allow(dead_code)]
    task_handle: Option<std::thread::JoinHandle<()>>,
}

impl XrtDetectorRuntime {
    pub fn port_handle(&self) -> &PortHandle {
        self.runtime_handle.port_handle()
    }

    pub fn pool(&self) -> &Arc<NDArrayPool> {
        &self.pool
    }

    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_output
    }

    pub fn connect_downstream(&self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.lock().add(sender);
    }
}

/// Create an XRT detector with actor-based runtime and acquisition task.
pub fn create_xrt_detector(
    port_name: &str,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
    sim_config: SimConfig,
) -> AsynResult<XrtDetectorRuntime> {
    let (acq_tx, acq_rx) = rt::command_channel(16);
    let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
    dirty.lock().set();

    let det = XrtDetector::new(port_name, size_x, size_y, max_memory, acq_tx, dirty.clone())?;

    let ad_params = det.ad.params;
    let xrt_params = det.xrt_params;
    let pool = det.ad.pool.clone();

    let (runtime_handle, _actor_jh) = create_port_runtime(det, RuntimeConfig::default())?;

    let shared_output = Arc::new(parking_lot::Mutex::new(array_output));
    let queued_counter = Arc::new(QueuedArrayCounter::new());

    let task_handle = start_acquisition_task(AcquisitionContext {
        acq_rx,
        port_handle: runtime_handle.port_handle().clone(),
        publisher: ArrayPublisher::new(shared_output.clone()),
        dirty,
        ad: ad_params,
        xrt: xrt_params,
        sim_config,
        queued_counter: queued_counter.clone(),
    });

    Ok(XrtDetectorRuntime {
        runtime_handle,
        ad_params,
        xrt_params,
        pool,
        array_output: shared_output,
        queued_counter,
        task_handle: Some(task_handle),
    })
}
