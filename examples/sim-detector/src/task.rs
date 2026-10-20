use asyn_rs::param::ParamValue;
use std::sync::Arc;
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::runtime as rt;

use ad_core_rs::color_layout::ColorLayout;
use ad_core_rs::ndarray_pool::{DriverArray, DriverPool, PublishedArray};
use ad_core_rs::plugin::channel::ArrayPublisher;
use ad_core_rs::roi::crop_roi;
use ad_core_rs::timestamp::EpicsTimestamp;

use crate::compute::{self, SineState};
use crate::params::{SimConfigSnapshot, SimDetectorParams};
use crate::types::DirtyFlags;

const MIN_DELAY_SECS: f64 = 1e-5;

/// Commands sent from the driver to the acquisition task.
pub enum AcqCommand {
    Start,
    Stop,
}

/// Bundled state for the acquisition task thread.
pub(crate) struct AcquisitionContext {
    pub acq_rx: rt::CommandReceiver<AcqCommand>,
    pub port_handle: PortHandle,
    pub publisher: ArrayPublisher,
    pub dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    pub ad: ADBaseParams,
    pub sim: SimDetectorParams,
    pub pool: DriverPool,
}

impl AcquisitionContext {
    /// Transition to Idle. Storing ADAcquire=0 is what clears ADAcquireBusy,
    /// held while ADWaitForPlugins waits on the queued arrays
    /// (asynNDArrayDriver.cpp:636-663).
    async fn end_acquisition(&self) {
        if let Err(e) = self
            .port_handle
            .set_params_and_notify(
                0,
                vec![
                    asyn_rs::request::ParamSetValue::new(
                        self.ad.status,
                        0,
                        ParamValue::Int32(ADStatus::Idle as i32),
                    ),
                    asyn_rs::request::ParamSetValue::new(self.ad.acquire, 0, ParamValue::Int32(0)),
                ],
            )
            .await
        {
            epics_libcom_rs::stderr_println!("set_params_and_notify error (end_acquisition): {e}");
        }
    }
}

/// C's pRaw_, pBackground_, pRamp_ and pPeak_: pool arrays, charged from the
/// reset that allocates them to the next (simDetector.cpp:625-638).
struct SimBuffers {
    raw: DriverArray,
    background: DriverArray,
    ramp: DriverArray,
    peak: DriverArray,
}

struct TaskState {
    rng: StdRng,
    /// `None` until the next frame allocates them.
    bufs: Option<SimBuffers>,
    sine_state: SineState,
    use_background: bool,
}

impl TaskState {
    fn new() -> Self {
        Self {
            rng: StdRng::from_os_rng(),
            bufs: None,
            sine_state: SineState::new(),
            use_background: false,
        }
    }

    fn apply_dirty(&mut self, dirty: &DirtyFlags, config: &SimConfigSnapshot) {
        if dirty.reallocate_buffers {
            // C releases the previous buffers before allocating the new ones.
            self.bufs = None;
            self.use_background = false;
        }

        let needs_rebuild = dirty.rebuild_background || dirty.reallocate_buffers;
        if needs_rebuild {
            self.use_background = config.noise != 0.0 || config.offset != 0.0;
        }
    }

    /// C computeImage: the image is computed into pRaw_ and converted into
    /// pArrays[0] (simDetector.cpp:694-699). `None` when the pool refused a
    /// buffer; it has printed its limit line.
    fn compute_frame(
        &mut self,
        pool: &DriverPool,
        config: &SimConfigSnapshot,
        reset: bool,
    ) -> Option<PublishedArray> {
        let layout = ColorLayout {
            color_mode: config.color_mode,
            size_x: config.max_size_x,
            size_y: config.max_size_y,
        };

        let b = match &mut self.bufs {
            Some(b) => b,
            None => {
                let alloc = || pool.alloc(layout.make_dims(), config.data_type).ok();
                self.bufs.insert(SimBuffers {
                    raw: alloc()?,
                    background: alloc()?,
                    ramp: alloc()?,
                    peak: alloc()?,
                })
            }
        };
        compute::compute_frame(
            &mut b.raw.data,
            &mut b.background.data,
            &mut b.ramp.data,
            &mut b.peak.data,
            &mut self.sine_state,
            &layout,
            config.sim_mode,
            &config.gains,
            &config.peak,
            &config.sine,
            config.offset,
            config.noise,
            self.use_background,
            reset,
            &mut self.rng,
        );

        let min_x = config.min_x.min(config.max_size_x.saturating_sub(1));
        let min_y = config.min_y.min(config.max_size_y.saturating_sub(1));
        let size_x = config.size_x.min(config.max_size_x - min_x).max(1);
        let size_y = config.size_y.min(config.max_size_y - min_y).max(1);

        pool.owner().replace_array(0, || {
            let mut frame = crop_roi(pool, &b.raw, &layout, min_x, min_y, size_x, size_y).ok()?;
            // C computeImage labels every image (simDetector.cpp:171); the
            // attribute is what `NDArray::info` reads the layout from.
            frame.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "Color mode",
                NDAttrSource::Driver,
                NDAttrValue::Int32(config.color_mode as i32),
            ));
            pool.adopt(frame)
        })
    }
}

/// Outcome of waiting for a stop signal during an exposure/period delay.
enum WaitOutcome {
    /// `Stop` command received — the asyn port is still up, so finalize the
    /// detector to Idle before breaking out of the acquisition loop.
    Stop,
    /// The command channel closed (IOC shutdown) — the asyn port is being torn
    /// down, so break WITHOUT the idle writeback that would race the dead port
    /// and log a spurious "actor channel closed" error.
    Shutdown,
    /// Timeout elapsed, or a stale `Start` arrived — keep acquiring.
    Continue,
}

/// Wait up to `duration` for a stop signal, distinguishing an explicit `Stop`
/// (finalize) from command-channel closure (IOC shutdown — skip the writeback).
async fn wait_for_stop(
    acq_rx: &mut rt::CommandReceiver<AcqCommand>,
    duration: Duration,
) -> WaitOutcome {
    match rt::timeout(duration, acq_rx.recv()).await {
        Ok(Some(AcqCommand::Stop)) => WaitOutcome::Stop,
        Ok(Some(AcqCommand::Start)) => WaitOutcome::Continue,
        Ok(None) => WaitOutcome::Shutdown, // channel closed = IOC shutdown
        Err(_) => WaitOutcome::Continue,   // timeout
    }
}

/// Start the acquisition task thread.
pub(crate) fn start_acquisition_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
    rt::run_thread_named("SimDetTask", move || acquisition_loop_async(ctx))
}

async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
    let mut task_state = TaskState::new();

    loop {
        // Wait for Start command
        match ctx.acq_rx.recv().await {
            Some(AcqCommand::Start) => {}
            Some(AcqCommand::Stop) => continue, // stale stop, ignore
            None => break,                      // channel closed = shutdown
        }

        // Initialize counters via PortHandle
        if let Err(e) = ctx
            .port_handle
            .set_params_and_notify(
                0,
                vec![
                    asyn_rs::request::ParamSetValue::new(
                        ctx.ad.num_images_counter,
                        0,
                        ParamValue::Int32(0),
                    ),
                    asyn_rs::request::ParamSetValue::new(
                        ctx.ad.status,
                        0,
                        ParamValue::Int32(ADStatus::Acquire as i32),
                    ),
                    asyn_rs::request::ParamSetValue::new(
                        ctx.ad.acquire_busy,
                        0,
                        ParamValue::Int32(1),
                    ),
                ],
            )
            .await
        {
            epics_libcom_rs::stderr_println!("set_params_and_notify error (acquire start): {e}");
        }

        let mut num_counter = 0;
        let mut array_counter = ctx
            .port_handle
            .read_int32(ctx.ad.base.array_counter, 0)
            .await
            .unwrap_or(0);

        // Read initial config
        let mut config =
            match SimConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.sim).await {
                Ok(cfg) => cfg,
                Err(_) => continue,
            };

        loop {
            let start_time = std::time::Instant::now();

            // Take dirty flags (shared with driver)
            let dirty_flags = ctx.dirty.lock().take();
            let reset = dirty_flags.any();

            // Only re-read config when parameters changed
            if reset {
                config =
                    match SimConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.sim)
                        .await
                    {
                        Ok(cfg) => cfg,
                        Err(_) => break,
                    };
            }
            task_state.apply_dirty(&dirty_flags, &config);

            // It stays in pArrays[0] through the exposure wait.
            let Some(frame) = task_state.compute_frame(&ctx.pool, &config, reset) else {
                continue;
            };

            // Exposure time sleep with stop interruption
            let elapsed = start_time.elapsed().as_secs_f64();
            let delay = (config.acquire_time - elapsed).max(MIN_DELAY_SECS);
            match wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(delay)).await {
                WaitOutcome::Stop => {
                    ctx.end_acquisition().await;
                    break;
                }
                WaitOutcome::Shutdown => break, // asyn port torn down; skip writeback
                WaitOutcome::Continue => {}
            }

            // Update counters (local to avoid blocking round-trips)
            num_counter += 1;
            array_counter += 1;

            // C updateTimeStamps (asynNDArrayDriver.cpp:832-836): the frame and
            // the callbacks after it carry the port's updated stamp.
            let stamp: EpicsTimestamp = ctx
                .port_handle
                .update_time_stamp()
                .await
                .unwrap_or_else(|_| std::time::SystemTime::now())
                .into();

            // C `simDetector::computeImage` publishes the converted array's
            // size beside the frame it belongs to (simDetector.cpp:709-712):
            // `NDArraySize` from `arrayInfo.totalBytes`, `NDArraySizeX/Y` from
            // the converted dims. Without them the detector port reports a
            // 0x0 frame of 0 bytes for the whole run.
            let info = frame.info();

            // Counter updates + callParamCallbacks always run (like C EPICS).
            // Only doCallbacksGenericPointer (publish) is gated by array_callbacks.
            if let Err(e) = ctx
                .port_handle
                .set_params_and_notify(
                    0,
                    vec![
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.array_counter,
                            0,
                            ParamValue::Int32(array_counter),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.num_images_counter,
                            0,
                            ParamValue::Int32(num_counter),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.array_size_x,
                            0,
                            ParamValue::Int32(info.x_size as i32),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.array_size_y,
                            0,
                            ParamValue::Int32(info.y_size as i32),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.array_size,
                            0,
                            ParamValue::Int32(info.total_bytes as i32),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.timestamp_rbv,
                            0,
                            ParamValue::Float64(stamp.as_f64()),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.epics_ts_sec,
                            0,
                            ParamValue::Int32(stamp.sec as i32),
                        ),
                        asyn_rs::request::ParamSetValue::new(
                            ctx.ad.base.epics_ts_nsec,
                            0,
                            ParamValue::Int32(stamp.nsec as i32),
                        ),
                    ],
                )
                .await
            {
                epics_libcom_rs::stderr_println!("set_params_and_notify error (frame update): {e}");
            }

            // C `getAttributes(pImage->pAttributeList)` (simDetector.cpp:828).
            let attributes = ctx
                .pool
                .owner()
                .evaluate_attributes()
                .await
                .unwrap_or_else(|e| {
                    epics_libcom_rs::stderr_println!("getAttributes error: {e}");
                    None
                });

            // simTask stamps pArrays[0] in place (simDetector.cpp:824-828).
            let Ok(frame) = ctx.pool.owner().refill(0, frame, |frame| {
                frame.unique_id = array_counter;
                frame.update_time_stamps(stamp);
                if let Some(attributes) = &attributes {
                    frame.attributes.copy_from(attributes);
                }
            }) else {
                continue;
            };
            if config.array_callbacks {
                ctx.publisher.publish(frame).await;
            }

            // Check stop conditions
            let image_mode = config.image_mode;
            let num_images = config.num_images;
            if image_mode == ImageMode::Single
                || (image_mode == ImageMode::Multiple && num_counter >= num_images)
            {
                ctx.end_acquisition().await;
                break;
            }

            // Period delay with stop interruption
            let total_elapsed = start_time.elapsed().as_secs_f64();
            let period_delay = config.acquire_period - total_elapsed;
            if period_delay > 0.0 {
                match wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(period_delay)).await {
                    WaitOutcome::Stop => {
                        ctx.end_acquisition().await;
                        break;
                    }
                    WaitOutcome::Shutdown => break, // asyn port torn down; skip writeback
                    WaitOutcome::Continue => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::create_sim_detector;
    use ad_core_rs::ndarray::NDDataType;
    use ad_core_rs::plugin::channel::NDArrayOutput;

    #[test]
    fn test_single_mode_auto_stop() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_TEST").unwrap(),
            32,
            32,
            NDDataType::UInt8 as i32,
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

        // Start acquisition
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(200));

        let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
        assert_eq!(
            acquire, 0,
            "acquire should be 0 after Single mode completes"
        );
        let counter = handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap();
        assert!(counter >= 1, "should have produced at least 1 frame");
    }

    /// C `computeImage` puts the colour axis where the mode says
    /// (simDetector.cpp:599-623): RGB1 first, RGB2 between X and Y, RGB3 last.
    #[test]
    fn each_rgb_mode_puts_the_colour_axis_where_c_does() {
        use ad_core_rs::driver::ColorMode;
        let (acq_tx, _acq_rx) = rt::command_channel(1);
        let dirty = Arc::new(parking_lot::Mutex::new(DirtyFlags::default()));
        let mut det = crate::driver::SimDetector::new(
            "SIM_RGB_DIMS",
            8,
            4,
            NDDataType::UInt8 as i32,
            1_000_000,
            acq_tx,
            dirty,
        )
        .unwrap();
        for (mode, dims) in [
            (ColorMode::Mono, vec![8, 4]),
            (ColorMode::RGB1, vec![3, 8, 4]),
            (ColorMode::RGB2, vec![8, 3, 4]),
            (ColorMode::RGB3, vec![8, 4, 3]),
        ] {
            det.ad
                .port_base
                .set_int32_param(det.ad.params.base.color_mode, 0, mode as i32)
                .unwrap();
            let config =
                SimConfigSnapshot::read_from(&det.ad.port_base, &det.ad.params, &det.sim_params)
                    .unwrap();
            let mut state = TaskState::new();
            let mut all = DirtyFlags::default();
            all.set_all();
            state.apply_dirty(&all, &config);
            let frame = state.compute_frame(&det.ad.pool, &config, true).unwrap();
            let got: Vec<usize> = frame.dims.iter().map(|d| d.size).collect();
            assert_eq!(got, dims, "{mode:?}");
        }
    }

    /// C `computeImage` converts into an array from the driver's own pool
    /// (simDetector.cpp:696), so `pArray->pDriver` is this driver and its
    /// queued count is the one a downstream queue counts on.
    #[test]
    fn a_frame_comes_from_the_driver_pool() {
        use ad_core_rs::plugin::channel::ndarray_channel;

        let (sender, mut receiver) = ndarray_channel("SIM_POOL_DOWN", 4);
        let mut output = NDArrayOutput::new();
        output.add(sender);
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_POOL").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
            1_000_000,
            output,
        )
        .unwrap();
        let handle = rt.port_handle();
        // The ArrayCallbacks record's PINI; the driver leaves it unset as C does.
        handle
            .write_int32_blocking(rt.ad_params.base.array_callbacks, 0, 1)
            .unwrap();
        handle
            .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
            .unwrap();
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();

        let frame = receiver.blocking_recv().unwrap();
        let owner = frame.owner().expect("frame from an owned pool");
        assert!(Arc::ptr_eq(&owner, rt.pool().owner()));
    }

    /// One image, then `PreAllocBuffers`: C keeps the image as `pArrays[0]`
    /// (simDetector.cpp:696) whether or not it calls back, so the pool copies
    /// it (asynNDArrayDriver.cpp:717-726).
    #[test]
    fn one_image_is_the_pre_alloc_template() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_PREALLOC").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
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
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < 1
            || h.read_int32_blocking(p.acquire, 0).unwrap() != 0
        {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::sleep(Duration::from_millis(5));
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

    /// C fills pArrays[0] in computeImage before the exposure wait
    /// (simDetector.cpp:697), so PreAllocBuffers during an exposure copies
    /// the image being exposed.
    #[test]
    fn pre_alloc_during_an_exposure_copies_the_image() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_PREALLOC_EXP").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
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
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < 1 {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(200));
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

    /// C releases pArrays[0] and converts the next image into it under the
    /// driver lock (simDetector.cpp:694-699), so free-running, a reader never
    /// finds the slot empty once the first image is there.
    #[test]
    fn a_free_running_acquisition_never_leaves_parrays0_empty() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_SLOT_FREE").unwrap(),
            1024,
            1024,
            NDDataType::UInt16 as i32,
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
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
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

    /// C computes into pRaw_, pBackground_, pRamp_ and pPeak_ from the pool
    /// (simDetector.cpp:635-638) beside pArrays[0], so one image then
    /// PreAllocBuffers(3) leaves 8 buffers, 3 free, as the C IOC reports.
    #[test]
    fn the_compute_buffers_come_from_the_pool() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_POOL_BUFS").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
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
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.read_int32_blocking(p.base.array_counter, 0).unwrap() < 1
            || h.read_int32_blocking(p.acquire, 0).unwrap() != 0
        {
            assert!(std::time::Instant::now() < deadline, "no image");
            std::thread::sleep(Duration::from_millis(5));
        }
        h.write_int32_blocking(p.base.pool_num_pre_alloc_buffers, 0, 3)
            .unwrap();
        h.write_int32_blocking(p.base.pool_pre_alloc, 0, 1).unwrap();
        h.write_int32_blocking(p.base.pool_poll_stats, 0, 1)
            .unwrap();
        let alloc = h.read_int32_blocking(p.base.pool_alloc_buffers, 0).unwrap();
        let free = h.read_int32_blocking(p.base.pool_free_buffers, 0).unwrap();
        assert_eq!((alloc, free), (8, 3));
    }

    /// C `simDetector::simTask` runs `getAttributes` on every image before
    /// its callback (simDetector.cpp:828), so the `NDAttributesFile` set and
    /// its `PARAM` values ride on each frame.
    #[test]
    fn a_frame_carries_the_attributes_file_set() {
        use ad_core_rs::attributes::NDAttrValue;
        use ad_core_rs::plugin::channel::ndarray_channel;

        let (sender, mut receiver) = ndarray_channel("SIM_ATTR_DOWN", 4);
        let mut output = NDArrayOutput::new();
        output.add(sender);
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_ATTR").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
            1_000_000,
            output,
        )
        .unwrap();
        let handle = rt.port_handle();
        // The ArrayCallbacks record's PINI; the driver leaves it unset as C does.
        handle
            .write_int32_blocking(rt.ad_params.base.array_callbacks, 0, 1)
            .unwrap();
        let file = rt.ad_params.base.attributes_file;
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
        </Attributes>"#;
        handle
            .submit_blocking(
                asyn_rs::request::RequestOp::OctetWrite { data: xml.into() },
                asyn_rs::user::AsynUser::new(file),
            )
            .unwrap();
        handle
            .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
            .unwrap();
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();

        let frame = receiver.blocking_recv().unwrap();
        assert_eq!(
            frame.attributes.get("Counter").map(|a| &a.value),
            Some(&NDAttrValue::Int32(frame.unique_id))
        );
    }

    #[test]
    fn test_continuous_mode_produces_frames() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_CONT").unwrap(),
            16,
            16,
            NDDataType::UInt8 as i32,
            1_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
        let handle = rt.port_handle();

        handle
            .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.002)
            .unwrap();

        // Start acquisition
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Stop acquisition
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 0)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let counter = handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap();
        assert!(
            counter >= 2,
            "should have produced multiple frames, got {}",
            counter
        );
    }

    #[test]
    fn test_stop_during_acquisition() {
        let rt = create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_STOP").unwrap(),
            8,
            8,
            NDDataType::UInt8 as i32,
            1_000_000,
            NDArrayOutput::new(),
        )
        .unwrap();
        let handle = rt.port_handle();

        handle
            .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.5)
            .unwrap();
        handle
            .write_float64_blocking(rt.ad_params.acquire_period, 0, 1.0)
            .unwrap();

        // Start acquisition
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 1)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        // Stop during long exposure
        handle
            .write_int32_blocking(rt.ad_params.acquire, 0, 0)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
        assert_eq!(acquire, 0);
    }
}
