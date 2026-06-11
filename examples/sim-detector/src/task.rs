use std::sync::Arc;
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::runtime as rt;

use ad_core_rs::color_layout::ColorLayout;
use ad_core_rs::plugin::channel::{ArrayPublisher, QueuedArrayCounter};
use ad_core_rs::roi::crop_roi;

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
    pub queued_counter: Arc<QueuedArrayCounter>,
}

impl AcquisitionContext {
    /// Drain in-flight arrays (if configured) and transition to Idle.
    async fn end_acquisition(&self, wait_for_plugins: bool) {
        if wait_for_plugins {
            self.queued_counter.wait_until_zero(Duration::from_secs(5));
        }
        if let Err(e) = self
            .port_handle
            .set_params_and_notify(
                0,
                vec![
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: self.ad.acquire_busy,
                        addr: 0,
                        value: 0,
                    },
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: self.ad.status,
                        addr: 0,
                        value: ADStatus::Idle as i32,
                    },
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: self.ad.acquire,
                        addr: 0,
                        value: 0,
                    },
                ],
            )
            .await
        {
            eprintln!("set_params_and_notify error (end_acquisition): {e}");
        }
    }
}

struct TaskState {
    rng: StdRng,
    raw_buf: NDDataBuffer,
    background_buf: NDDataBuffer,
    ramp_buf: NDDataBuffer,
    peak_buf: NDDataBuffer,
    sine_state: SineState,
    use_background: bool,
}

impl TaskState {
    fn new() -> Self {
        Self {
            rng: StdRng::from_os_rng(),
            raw_buf: NDDataBuffer::zeros(ad_core_rs::ndarray::NDDataType::UInt8, 0),
            background_buf: NDDataBuffer::zeros(ad_core_rs::ndarray::NDDataType::UInt8, 0),
            ramp_buf: NDDataBuffer::zeros(ad_core_rs::ndarray::NDDataType::UInt8, 0),
            peak_buf: NDDataBuffer::zeros(ad_core_rs::ndarray::NDDataType::UInt8, 0),
            sine_state: SineState::new(),
            use_background: false,
        }
    }

    fn apply_dirty(&mut self, dirty: &DirtyFlags, config: &SimConfigSnapshot) {
        if dirty.reallocate_buffers {
            let layout = ColorLayout {
                color_mode: config.color_mode,
                size_x: config.max_size_x,
                size_y: config.max_size_y,
            };
            let n = layout.num_elements();
            self.raw_buf = NDDataBuffer::zeros(config.data_type, n);
            self.background_buf = NDDataBuffer::zeros(config.data_type, n);
            self.ramp_buf = NDDataBuffer::zeros(config.data_type, n);
            self.peak_buf = NDDataBuffer::zeros(config.data_type, n);
            self.use_background = false;
        }

        let needs_rebuild = dirty.rebuild_background || dirty.reallocate_buffers;
        if needs_rebuild {
            self.use_background = config.noise != 0.0 || config.offset != 0.0;
        }
    }

    fn compute_frame(&mut self, config: &SimConfigSnapshot, reset: bool) -> NDArray {
        let layout = ColorLayout {
            color_mode: config.color_mode,
            size_x: config.max_size_x,
            size_y: config.max_size_y,
        };

        compute::compute_frame(
            &mut self.raw_buf,
            &mut self.background_buf,
            &mut self.ramp_buf,
            &mut self.peak_buf,
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

        // Build a temporary NDArray wrapper for crop_roi (which now takes &NDArray)
        let mut temp_arr = NDArray::new(layout.make_dims(), self.raw_buf.data_type());
        temp_arr.data = self.raw_buf.clone();
        crop_roi(&temp_arr, &layout, min_x, min_y, size_x, size_y)
            .expect("crop_roi failed on uncompressed data")
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
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: ctx.ad.num_images_counter,
                        addr: 0,
                        value: 0,
                    },
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: ctx.ad.status,
                        addr: 0,
                        value: ADStatus::Acquire as i32,
                    },
                    asyn_rs::request::ParamSetValue::Int32 {
                        reason: ctx.ad.acquire_busy,
                        addr: 0,
                        value: 1,
                    },
                ],
            )
            .await
        {
            eprintln!("set_params_and_notify error (acquire start): {e}");
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

            let mut frame = task_state.compute_frame(&config, reset);

            // Exposure time sleep with stop interruption
            let elapsed = start_time.elapsed().as_secs_f64();
            let delay = (config.acquire_time - elapsed).max(MIN_DELAY_SECS);
            match wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(delay)).await {
                WaitOutcome::Stop => {
                    ctx.end_acquisition(config.wait_for_plugins).await;
                    break;
                }
                WaitOutcome::Shutdown => break, // asyn port torn down; skip writeback
                WaitOutcome::Continue => {}
            }

            // Update counters (local to avoid blocking round-trips)
            num_counter += 1;
            array_counter += 1;

            frame.unique_id = array_counter;
            frame.timestamp = ad_core_rs::timestamp::EpicsTimestamp::now();

            // Counter updates + callParamCallbacks always run (like C EPICS).
            // Only doCallbacksGenericPointer (publish) is gated by array_callbacks.
            if let Err(e) = ctx
                .port_handle
                .set_params_and_notify(
                    0,
                    vec![
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_counter,
                            addr: 0,
                            value: array_counter,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.num_images_counter,
                            addr: 0,
                            value: num_counter,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.ad.base.timestamp_rbv,
                            addr: 0,
                            value: frame.timestamp.as_f64(),
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.epics_ts_sec,
                            addr: 0,
                            value: frame.timestamp.sec as i32,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.epics_ts_nsec,
                            addr: 0,
                            value: frame.timestamp.nsec as i32,
                        },
                    ],
                )
                .await
            {
                eprintln!("set_params_and_notify error (frame update): {e}");
            }

            if config.array_callbacks {
                ctx.publisher.publish(Arc::new(frame)).await;
            }

            // Check stop conditions
            let image_mode = config.image_mode;
            let num_images = config.num_images;
            if image_mode == ImageMode::Single
                || (image_mode == ImageMode::Multiple && num_counter >= num_images)
            {
                ctx.end_acquisition(config.wait_for_plugins).await;
                break;
            }

            // Period delay with stop interruption
            let total_elapsed = start_time.elapsed().as_secs_f64();
            let period_delay = config.acquire_period - total_elapsed;
            if period_delay > 0.0 {
                match wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(period_delay)).await {
                    WaitOutcome::Stop => {
                        ctx.end_acquisition(config.wait_for_plugins).await;
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
    use ad_core_rs::plugin::channel::NDArrayOutput;

    #[test]
    fn test_single_mode_auto_stop() {
        let rt = create_sim_detector("SIM_TEST", 32, 32, 1_000_000, NDArrayOutput::new()).unwrap();
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

    #[test]
    fn test_continuous_mode_produces_frames() {
        let rt = create_sim_detector("SIM_CONT", 16, 16, 1_000_000, NDArrayOutput::new()).unwrap();
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
        let rt = create_sim_detector("SIM_STOP", 8, 8, 1_000_000, NDArrayOutput::new()).unwrap();
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
