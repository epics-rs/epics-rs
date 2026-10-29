use asyn_rs::param::ParamValue;
use std::sync::Arc;
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDDataType, NDDataViewMut, NDDimension};
use ad_core_rs::ndarray_pool::DriverPool;
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

/// Sum `bx` x `by` pixel blocks of the `width`-wide `img` into `out`
/// (`out_w` wide), rounding photon counts to u16 (clamped to 0..65535).
/// Every element of `out` is written. Unbinned, a block is one pixel.
fn bin_into_u16(img: &[f64], width: usize, bx: usize, by: usize, out_w: usize, out: &mut [u16]) {
    for (i, o) in out.iter_mut().enumerate() {
        let (row, col) = (i / out_w, i % out_w);
        let mut sum = 0.0;
        for dy in 0..by {
            for dx in 0..bx {
                sum += img[(row * by + dy) * width + col * bx + dx];
            }
        }
        *o = sum.round().clamp(0.0, 65535.0) as u16;
    }
}

/// Start the acquisition task thread.
pub(crate) fn start_acquisition_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
    rt::run_thread_named("MovingDotTask", move || acquisition_loop_async(ctx))
}

async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
    let mut rng = StdRng::from_os_rng();
    let mut img_data = Vec::new();

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
                // If the read fails (e.g. port busy with CP link writes during motor move),
                // keep the previous config rather than aborting acquisition.
                if let Ok(cfg) =
                    MovingDotConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.dot)
                        .await
                {
                    config = cfg;
                }
            }

            // Generate the image
            let width = config.size_x;
            let height = config.size_y;
            img_data.resize(width * height, 0.0);
            physics::moving_dot_image_with_config(
                &mut img_data,
                width,
                height,
                config.motor_x,
                config.motor_y,
                config.beam_current,
                config.acquire_time,
                config.shutter_open,
                &mut rng,
                &ctx.image_config,
            );

            // Apply slit aperture mask (zero out pixels outside the aperture)
            let sl = config.slit_left.max(0.0) as usize;
            let sr = (config.slit_right as usize).min(width);
            let st = config.slit_top.max(0.0) as usize;
            let sb = (config.slit_bottom as usize).min(height);
            for row in 0..height {
                for col in 0..width {
                    if row < st || row >= sb || col < sl || col >= sr {
                        img_data[row * width + col] = 0.0;
                    }
                }
            }

            let (bx, by) = (config.bin_x, config.bin_y);
            let (out_w, out_h) = (width / bx, height / by);
            let dims = vec![NDDimension::new(out_w), NDDimension::new(out_h)];
            // As C simDetector, the last image is released and the next is
            // allocated in its place under one lock (simDetector.cpp:694-699);
            // it stays there through the exposure wait. A refused wrap has
            // printed the pool's limit line.
            let Some(frame) = ctx.pool.owner().replace_array(0, || {
                let mut frame = ctx.pool.alloc(dims, NDDataType::UInt16).ok()?;
                let mut fill = frame.fill();
                let NDDataViewMut::U16(out) = fill.data_mut().view_mut() else {
                    unreachable!("the frame was allocated as UInt16")
                };
                bin_into_u16(&img_data, width, bx, by, out_w, out);
                Some(frame)
            }) else {
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

            // Update parameters and fire callbacks in one atomic operation.
            // Using set_params_and_notify instead of write_int32_no_wait avoids
            // going through the driver's writeInt32 (which is for external writes)
            // and directly sets params + fires interrupts for I/O Intr records.
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
                frame.set_unique_id(array_counter);
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

    /// Against the per-frame `Vec` binning it replaced: a reused frame
    /// buffer comes back with the same counts, unbinned and binned.
    #[test]
    fn bin_into_u16_matches_the_old_binning() {
        let img: Vec<f64> = (0..15).map(|i| i as f64 * 10.25 - 20.0).collect();
        let mut clipped = img.clone();
        clipped[7] = 70000.0;
        for (src, bx, by, want) in [
            (
                &clipped,
                1,
                1,
                &[
                    0, 0, 1, 11, 21, 31, 42, 65535, 62, 72, 83, 93, 103, 113, 124,
                ][..],
            ),
            (&img, 2, 1, &[0, 11, 73, 114, 175, 216]),
            (&img, 2, 2, &[43, 125]),
            (&img, 3, 2, &[95]),
            (&img, 1, 3, &[94, 125, 155, 186, 217]),
        ] {
            let (out_w, out_h) = (5 / bx, 3 / by);
            let mut out = vec![u16::MAX; out_w * out_h];
            bin_into_u16(src, 5, bx, by, out_w, &mut out);
            assert_eq!(out, want, "{bx}x{by}");
        }
    }
}
