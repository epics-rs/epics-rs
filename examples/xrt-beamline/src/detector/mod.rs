pub mod params;
pub mod task;
pub mod types;

use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::PortClaim;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::user::AsynUser;

use ad_core_rs::driver::{ADDriver, ADDriverBase, ImageMode};
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput, NDArraySender};
use ad_core_rs::plugin::runtime::ASYN_NONE;
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
        // ASYN_CANBLOCK=0: no C driver; acquisition runs on its own thread like simDetector.
        let mut ad = ADDriverBase::new(
            port_name,
            1,
            max_memory,
            ASYN_NONE,
            InterfaceMask::UNMIGRATED_SUBCLASS,
            InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS,
        )?;
        let xrt_params = XrtDetectorParams::create(&mut ad.port_base)?;
        let motor_param_set = xrt_params.motor_params();

        let base = &mut ad.port_base;
        base.set_string_param(ad.params.base.manufacturer, 0, "XRT Beamline")?;
        base.set_string_param(ad.params.base.model, 0, "Ray Tracing Simulator")?;
        // simDetector.cpp:1102-1103, after ADDriver's seed of 1.
        base.set_int32_param(ad.params.max_size_x, 0, size_x)?;
        base.set_int32_param(ad.params.max_size_y, 0, size_y)?;

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
        } else if reason < self.xrt_params.und_gap {
            // und_gap is the driver's first parameter; ADDriver::writeInt32
            // owns the ones below it (ADDriver.cpp:84-116).
            return self.ad_write_int32(user, value);
        } else {
            self.ad
                .port_base
                .set_int32_param(reason, user.addr, value)?;
            self.ad.port_base.call_param_callback(0, reason)?;
        }

        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.ad
            .port_base
            .set_float64_param(reason, user.addr, value)?;

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
    pool: DriverPool,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    #[allow(dead_code)]
    task_handle: Option<std::thread::JoinHandle<()>>,
}

impl XrtDetectorRuntime {
    pub fn port_handle(&self) -> &PortHandle {
        self.runtime_handle.port_handle()
    }

    pub fn pool(&self) -> &DriverPool {
        &self.pool
    }

    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_output
    }

    pub fn connect_downstream(&self, sender: NDArraySender) {
        self.array_output.lock().add(sender);
    }
}

/// Create an XRT detector with actor-based runtime and acquisition task.
pub fn create_xrt_detector(
    claim: &PortClaim,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
    sim_config: SimConfig,
) -> AsynResult<XrtDetectorRuntime> {
    let port_name = claim.name();
    let (acq_tx, acq_rx) = rt::command_channel(16);
    let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
    dirty.lock().set();

    let det = XrtDetector::new(port_name, size_x, size_y, max_memory, acq_tx, dirty.clone())?;

    let ad_params = det.ad.params;
    let xrt_params = det.xrt_params;
    let pool = det.ad.pool.clone();

    let (runtime_handle, _actor_jh) = create_port_runtime(claim, det, RuntimeConfig::default())?;

    let shared_output = Arc::new(parking_lot::Mutex::new(array_output));

    let task_handle = start_acquisition_task(AcquisitionContext {
        acq_rx,
        port_handle: runtime_handle.port_handle().clone(),
        publisher: ArrayPublisher::new(shared_output.clone()),
        dirty,
        ad: ad_params,
        xrt: xrt_params,
        sim_config,
        pool: pool.clone(),
    });

    Ok(XrtDetectorRuntime {
        runtime_handle,
        ad_params,
        xrt_params,
        pool,
        array_output: shared_output,
        task_handle: Some(task_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A base-class parameter reaches ADDriver::writeInt32, whose pool branch
    /// clears the PreAllocBuffers busy command (asynNDArrayDriver.cpp:686-688).
    #[test]
    fn test_write_int32_hands_a_base_param_to_ad_driver() {
        let (acq_tx, _acq_rx) = rt::command_channel(1);
        let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
        let mut det = XrtDetector::new("XRT_PREALLOC", 8, 8, 1_000_000, acq_tx, dirty).unwrap();
        let pre_alloc = det.ad.params.base.pool_pre_alloc;
        det.write_int32(&mut AsynUser::new(pre_alloc), 1).unwrap();
        assert_eq!(det.ad.port_base.get_int32_param(pre_alloc, 0).unwrap(), 0);
    }

    /// C releases pArrays[0] and converts the next image into it under the
    /// driver lock (simDetector.cpp:694-699), so free-running, a reader never
    /// finds the slot empty once the first image is there.
    #[test]
    fn a_free_running_acquisition_never_leaves_parrays0_empty() {
        let rt = create_xrt_detector(
            &asyn_rs::registry::claim_port("XRT_SLOT_FREE").unwrap(),
            16,
            12,
            64_000_000,
            NDArrayOutput::new(),
            SimConfig {
                screen_nx: 16,
                screen_nz: 12,
                ..SimConfig::default()
            },
        )
        .unwrap();
        let h = rt.port_handle();
        let p = rt.ad_params;
        h.write_int32_blocking(p.image_mode, 0, ImageMode::Continuous as i32)
            .unwrap();
        h.write_float64_blocking(p.acquire_time, 0, 0.0).unwrap();
        h.write_int32_blocking(p.acquire, 0, 1).unwrap();
        let owner = rt.pool().owner();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while owner.array(0).is_none() {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::yield_now();
        }
        let first = h.read_int32_blocking(p.base.array_counter, 0).unwrap();
        let (mut samples, mut empty) = (0u32, 0u32);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < first + 3 {
            assert!(std::time::Instant::now() < deadline, "no third image");
            for _ in 0..100 {
                samples += 1;
                empty += u32::from(owner.array(0).is_none());
                std::thread::yield_now();
            }
        }
        h.write_int32_blocking(p.acquire, 0, 0).unwrap();
        assert_eq!(
            empty, 0,
            "{empty} of {samples} samples found pArrays[0] empty"
        );
    }

    /// One image, then `PreAllocBuffers`: the image is kept as `pArrays[0]`,
    /// as C simDetector does (simDetector.cpp:696), so the pool copies it
    /// (asynNDArrayDriver.cpp:717-726).
    #[test]
    fn one_image_is_the_pre_alloc_template() {
        let sim = SimConfig {
            screen_nx: 16,
            screen_nz: 12,
            ..SimConfig::default()
        };
        let rt = create_xrt_detector(
            &asyn_rs::registry::claim_port("XRT_PREALLOC_TEMPLATE").unwrap(),
            16,
            12,
            1_000_000,
            NDArrayOutput::new(),
            sim,
        )
        .unwrap();
        let h = rt.port_handle();
        let p = rt.ad_params;
        h.write_int32_blocking(p.image_mode, 0, ImageMode::Single as i32)
            .unwrap();
        h.write_float64_blocking(p.acquire_time, 0, 0.001).unwrap();
        h.write_int32_blocking(p.acquire, 0, 1).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < 1
            || h.read_int32_blocking(p.acquire, 0).unwrap() != 0
        {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.write_int32_blocking(p.base.pool_poll_stats, 0, 1)
            .unwrap();
        let before = h.read_int32_blocking(p.base.pool_alloc_buffers, 0).unwrap();
        h.write_int32_blocking(p.base.pool_num_pre_alloc_buffers, 0, 2)
            .unwrap();
        h.write_int32_blocking(p.base.pool_pre_alloc, 0, 1).unwrap();
        let after = h.read_int32_blocking(p.base.pool_alloc_buffers, 0).unwrap();
        assert_eq!(after - before, 2);
    }
}
