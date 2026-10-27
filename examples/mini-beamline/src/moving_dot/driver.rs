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

use crate::physics::MovingDotImageConfig;

use super::params::MovingDotParams;
use super::task::{AcqCommand, AcquisitionContext, start_acquisition_task};
use super::types::DirtyFlags;

/// MovingDot area detector driver.
pub struct MovingDotDetector {
    pub ad: ADDriverBase,
    pub dot_params: MovingDotParams,
    dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    acq_tx: rt::CommandSender<AcqCommand>,
}

impl MovingDotDetector {
    pub fn new(
        port_name: &str,
        max_size_x: i32,
        max_size_y: i32,
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
            // rs-only example port with no C driver: its params are all
            // covered by ADDriver's and asynNDArrayDriver's own masks.
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        )?;
        let dot_params = MovingDotParams::create(&mut ad.port_base)?;

        // Set defaults
        let base = &mut ad.port_base;
        base.set_string_param(ad.params.base.manufacturer, 0, "Mini Beamline")?;
        base.set_string_param(ad.params.base.model, 0, "Moving Dot")?;
        // simDetector.cpp:1102-1103, after ADDriver's seed of 1.
        base.set_int32_param(ad.params.max_size_x, 0, max_size_x)?;
        base.set_int32_param(ad.params.max_size_y, 0, max_size_y)?;

        base.set_float64_param(ad.params.acquire_time, 0, 0.1)?;
        base.set_float64_param(ad.params.acquire_period, 0, 0.5)?;
        base.set_int32_param(ad.params.image_mode, 0, ImageMode::Continuous as i32)?;
        base.set_int32_param(ad.params.num_images, 0, 100)?;

        base.set_int32_param(ad.params.base.data_type, 0, 3)?; // UInt16
        // The read/write geometry parameters belong to the concrete driver:
        // `ADDriverBase` seeds only what C's ADDriver constructor seeds
        // (ADDriver.cpp:180-191), because SizeX/SizeY/BinX/BinY carry
        // PINI="YES" in ADBase.template and a driver-side seed would override
        // the DB and autosave.
        base.set_int32_param(ad.params.size_x, 0, max_size_x)?;
        base.set_int32_param(ad.params.size_y, 0, max_size_y)?;
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
                .set_int32_param(reason, user.addr, value)?;
            if reason == self.dot_params.shutter_open
                || reason == self.ad.params.bin_x
                || reason == self.ad.params.bin_y
            {
                self.dirty.lock().set();
            }
            // motor_x_pos is the driver's first parameter; ADDriver::writeInt32
            // owns the ones below it (ADDriver.cpp:84-116).
            if reason < self.dot_params.motor_x_pos {
                return self.ad_write_int32(user, value);
            }
            self.ad.port_base.call_param_callback(0, reason)?;
        }

        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.ad
            .port_base
            .set_float64_param(reason, user.addr, value)?;

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
    pool: DriverPool,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    #[allow(dead_code)]
    task_handle: Option<std::thread::JoinHandle<()>>,
}

impl MovingDotRuntime {
    pub fn port_handle(&self) -> &PortHandle {
        self.runtime_handle.port_handle()
    }

    pub fn pool(&self) -> &DriverPool {
        &self.pool
    }

    /// The shared array output (for building a DriverContext).
    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_output
    }

    pub fn connect_downstream(&self, sender: NDArraySender) {
        self.array_output.lock().add(sender);
    }
}

/// Create a MovingDot detector with actor-based runtime and acquisition task.
pub fn create_moving_dot(
    claim: &PortClaim,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
) -> AsynResult<MovingDotRuntime> {
    create_moving_dot_with_config(
        claim,
        size_x,
        size_y,
        max_memory,
        array_output,
        MovingDotImageConfig::default(),
    )
}

/// Create a MovingDot detector with custom image generation config.
pub fn create_moving_dot_with_config(
    claim: &PortClaim,
    size_x: i32,
    size_y: i32,
    max_memory: usize,
    array_output: NDArrayOutput,
    image_config: MovingDotImageConfig,
) -> AsynResult<MovingDotRuntime> {
    let port_name = claim.name();
    let (acq_tx, acq_rx) = rt::command_channel(16);
    let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
    dirty.lock().set();

    let det = MovingDotDetector::new(port_name, size_x, size_y, max_memory, acq_tx, dirty.clone())?;

    let ad_params = det.ad.params;
    let dot_params = det.dot_params;
    let pool = det.ad.pool.clone();

    let (runtime_handle, _actor_jh) = create_port_runtime(claim, det, RuntimeConfig::default())?;

    let shared_output = Arc::new(parking_lot::Mutex::new(array_output));

    let task_handle = start_acquisition_task(AcquisitionContext {
        acq_rx,
        port_handle: runtime_handle.port_handle().clone(),
        publisher: ArrayPublisher::new(shared_output.clone()),
        dirty,
        ad: ad_params,
        dot: dot_params,
        image_config,
        pool: pool.clone(),
    });

    Ok(MovingDotRuntime {
        runtime_handle,
        ad_params,
        dot_params,
        pool,
        array_output: shared_output,
        task_handle: Some(task_handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::plugin::channel::NDArrayOutput;

    /// No masks beyond ADDriver.cpp:131-132 and asynNDArrayDriver.cpp:866-867.
    #[test]
    fn test_port_carries_only_the_addriver_masks() {
        let (acq_tx, _acq_rx) = rt::command_channel(1);
        let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
        let det = MovingDotDetector::new("DOT_MASKS", 8, 8, 1_000_000, acq_tx, dirty).unwrap();
        let interrupts = InterfaceMask::INT32
            | InterfaceMask::FLOAT64
            | InterfaceMask::OCTET
            | InterfaceMask::INT32_ARRAY
            | InterfaceMask::GENERIC_POINTER;
        assert_eq!(det.ad.port_base.interrupt_mask(), interrupts);
        assert_eq!(
            det.ad.port_base.interface_mask(),
            interrupts | InterfaceMask::DRV_USER | InterfaceMask::COMMON
        );
    }

    /// A base-class parameter reaches ADDriver::writeInt32, whose pool branch
    /// clears the PreAllocBuffers busy command (asynNDArrayDriver.cpp:686-688).
    #[test]
    fn test_write_int32_hands_a_base_param_to_ad_driver() {
        let (acq_tx, _acq_rx) = rt::command_channel(1);
        let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
        let mut det =
            MovingDotDetector::new("DOT_PREALLOC", 8, 8, 1_000_000, acq_tx, dirty).unwrap();
        let pre_alloc = det.ad.params.base.pool_pre_alloc;
        det.write_int32(&mut AsynUser::new(pre_alloc), 1).unwrap();
        assert_eq!(det.ad.port_base.get_int32_param(pre_alloc, 0).unwrap(), 0);
    }

    #[test]
    fn test_create_moving_dot() {
        let rt = create_moving_dot(
            &asyn_rs::registry::claim_port("DOT_TEST").unwrap(),
            640,
            480,
            10_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
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

    /// One image, then `PreAllocBuffers`: the image is kept as `pArrays[0]`,
    /// as C simDetector does (simDetector.cpp:696), so the pool copies it
    /// (asynNDArrayDriver.cpp:717-726).
    #[test]
    fn one_image_is_the_pre_alloc_template() {
        let rt = create_moving_dot(
            &asyn_rs::registry::claim_port("DOT_PREALLOC_TEMPLATE").unwrap(),
            16,
            12,
            1_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
        let h = rt.port_handle();
        let p = rt.ad_params;
        h.write_int32_blocking(p.image_mode, 0, ImageMode::Single as i32)
            .unwrap();
        h.write_float64_blocking(p.acquire_time, 0, 0.001).unwrap();
        h.write_int32_blocking(p.acquire, 0, 1).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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

    /// C releases pArrays[0] and converts the next image into it under the
    /// driver lock (simDetector.cpp:694-699), so free-running, a reader never
    /// finds the slot empty once the first image is there.
    #[test]
    fn a_free_running_acquisition_never_leaves_parrays0_empty() {
        let rt = create_moving_dot(
            &asyn_rs::registry::claim_port("DOT_SLOT_FREE").unwrap(),
            1024,
            1024,
            64_000_000,
            NDArrayOutput::new(),
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

    /// C fills pArrays[0] before the exposure wait (simDetector.cpp:697),
    /// so PreAllocBuffers during an exposure copies the image being exposed.
    #[test]
    fn pre_alloc_during_an_exposure_copies_the_image() {
        let rt = create_moving_dot(
            &asyn_rs::registry::claim_port("DOT_PREALLOC_EXPOSURE").unwrap(),
            16,
            12,
            1_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
        let h = rt.port_handle();
        let p = rt.ad_params;
        h.write_int32_blocking(p.image_mode, 0, ImageMode::Continuous as i32)
            .unwrap();
        h.write_float64_blocking(p.acquire_time, 0, 1.0).unwrap();
        h.write_int32_blocking(p.acquire, 0, 1).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < 1 {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        h.write_int32_blocking(p.base.pool_poll_stats, 0, 1)
            .unwrap();
        let before = h.read_int32_blocking(p.base.pool_alloc_buffers, 0).unwrap();
        h.write_int32_blocking(p.base.pool_num_pre_alloc_buffers, 0, 2)
            .unwrap();
        h.write_int32_blocking(p.base.pool_pre_alloc, 0, 1).unwrap();
        let after = h.read_int32_blocking(p.base.pool_alloc_buffers, 0).unwrap();
        assert_eq!(h.read_int32_blocking(p.base.array_counter, 0).unwrap(), 1);
        h.write_int32_blocking(p.acquire, 0, 0).unwrap();
        assert_eq!(after - before, 2);
    }

    #[test]
    fn test_single_frame_acquisition() {
        let rt = create_moving_dot(
            &asyn_rs::registry::claim_port("DOT_SINGLE").unwrap(),
            64,
            48,
            1_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "acquire should be 0 after Single mode"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let counter = handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap();
        assert!(counter >= 1, "should have produced at least 1 frame");
    }
}
