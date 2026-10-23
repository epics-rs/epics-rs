use asyn_rs::param::ParamValue;
use std::sync::Arc;
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDDataType, NDDataViewMut, NDDimension};
use ad_core_rs::ndarray_pool::{DriverPool, PublishedArray};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::ArrayPublisher;
use ad_core_rs::runtime as rt;
use ad_core_rs::timestamp::EpicsTimestamp;

use crate::physics::{self, MovingDotImageConfig};

use super::params::{MovingDotConfigSnapshot, MovingDotParams};
use super::types::DirtyFlags;

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
    pub dot: MovingDotParams,
    pub image_config: MovingDotImageConfig,
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
    rt::run_thread_named("MovingDotTask", move || acquisition_loop_async(ctx))
}

/// The image is computed into `raw`, C simDetector's pRaw_, with the slot's
/// lock free for a port write below FIRST_AD_PARAM, which reads `pArrays[0]`
/// under it; the last image is then released and the next copied into its
/// place (simDetector.cpp:694-699). `None` when the pool refused it, which
/// empties the slot as C's refused alloc does; it has printed the pool's
/// limit line.
fn compute_image(
    pool: &DriverPool,
    config: &MovingDotConfigSnapshot,
    rng: &mut StdRng,
    image_config: &MovingDotImageConfig,
    raw: &mut Vec<f64>,
) -> Option<PublishedArray> {
    let width = config.size_x;
    let height = config.size_y;
    raw.resize(width * height, 0.0);
    physics::moving_dot_image_with_config(
        raw,
        width,
        height,
        config.motor_x,
        config.motor_y,
        config.beam_current,
        config.acquire_time,
        config.shutter_open,
        rng,
        image_config,
    );
    // dims[0] is X, as ADSimDetector's mono xDim (simDetector.cpp:602, :632).
    let dims = vec![NDDimension::new(width), NDDimension::new(height)];
    pool.owner().replace_array(0, || {
        let mut frame = pool.alloc(dims, NDDataType::Float64).ok()?;
        let NDDataViewMut::F64(img) = frame.data_mut().view_mut() else {
            unreachable!("the frame was allocated as Float64")
        };
        img.copy_from_slice(raw);
        Some(frame)
    })
}

async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
    let mut rng = StdRng::from_os_rng();
    let mut raw = Vec::new();

    loop {
        // Wait for Start command
        match ctx.acq_rx.recv().await {
            Some(AcqCommand::Start) => {}
            Some(AcqCommand::Stop) => continue,
            None => break,
        }

        // Initialize counters
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
            match MovingDotConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.dot)
                .await
            {
                Ok(cfg) => cfg,
                Err(_) => continue,
            };

        loop {
            let start_time = std::time::Instant::now();

            // Take dirty flags
            let dirty_flags = ctx.dirty.lock().take();
            let reset = dirty_flags.any;

            if reset {
                config = match MovingDotConfigSnapshot::read_via_handle(
                    &ctx.port_handle,
                    &ctx.ad,
                    &ctx.dot,
                )
                .await
                {
                    Ok(cfg) => cfg,
                    Err(_) => break,
                };
            }

            // Generate the image; it stays in pArrays[0] through the
            // exposure wait.
            let Some(frame) =
                compute_image(&ctx.pool, &config, &mut rng, &ctx.image_config, &mut raw)
            else {
                continue;
            };

            // Exposure time delay with stop interruption
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

            // Update counters
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

            // Stamped in place in pArrays[0] (simDetector.cpp:824-825).
            let Ok(frame) = ctx.pool.owner().refill(0, frame, |frame| {
                frame.unique_id = array_counter;
                frame.update_time_stamps(stamp);
            }) else {
                continue;
            };
            if config.array_callbacks {
                ctx.publisher.publish(frame).await;
            }

            // Check stop conditions
            if config.image_mode == ImageMode::Single
                || (config.image_mode == ImageMode::Multiple && num_counter >= config.num_images)
            {
                ctx.end_acquisition().await;
                break;
            }

            // Period delay
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
    use ad_core_rs::ndarray::NDDataView;

    fn config(size_x: usize, size_y: usize) -> MovingDotConfigSnapshot {
        MovingDotConfigSnapshot {
            motor_x: 1.0,
            motor_y: -1.0,
            beam_current: 500.0,
            shutter_open: true,
            acquire_time: 0.1,
            acquire_period: 0.1,
            image_mode: ImageMode::Single,
            num_images: 1,
            array_callbacks: true,
            wait_for_plugins: false,
            size_x,
            size_y,
        }
    }

    /// `dims[0]` is SizeX, as ADSimDetector's, so a 640x480 frame reports
    /// ArraySize0 640, and pixel (x, y) is the generator's row y, column x.
    #[test]
    fn a_frame_is_size_x_wide() {
        let (width, height) = (6, 4);
        let pool = DriverPool::standalone(0);
        let mut rng = StdRng::seed_from_u64(3);
        let frame = compute_image(
            &pool,
            &config(width, height),
            &mut rng,
            &MovingDotImageConfig::default(),
            &mut Vec::new(),
        )
        .unwrap();

        let sizes: Vec<_> = frame.dims.iter().map(|d| d.size).collect();
        assert_eq!(sizes, [width, height]);
        let info = frame.info();
        assert_eq!((info.x_size, info.y_size), (width, height));

        let want = physics::moving_dot_image(
            width,
            height,
            1.0,
            -1.0,
            500.0,
            0.1,
            true,
            &mut StdRng::seed_from_u64(3),
        );
        let NDDataView::F64(got) = frame.data().view() else {
            panic!("{:?}", frame.data().data_type());
        };
        for y in 0..height {
            for x in 0..width {
                assert_eq!(got[y * sizes[0] + x], want[y * width + x], "({x}, {y})");
            }
        }
    }

    /// The image is computed outside the slot's lock and copied into the
    /// buffer the released last image leaves in the pool, so every frame
    /// reuses one buffer, as C's convert into `pArrays[0]` does.
    #[test]
    fn each_image_reuses_the_released_buffer() {
        let pool = DriverPool::standalone(0);
        let mut rng = StdRng::seed_from_u64(3);
        let mut raw = Vec::new();
        let mut image = || {
            let frame = compute_image(
                &pool,
                &config(6, 4),
                &mut rng,
                &MovingDotImageConfig::default(),
                &mut raw,
            )
            .unwrap();
            frame.data().as_u8_slice().as_ptr()
        };
        let first = image();
        assert_eq!([image(), image()], [first; 2]);
    }
}
