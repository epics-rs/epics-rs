use std::sync::Arc;
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::{ArrayPublisher, QueuedArrayCounter};
use ad_core_rs::runtime as rt;

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

/// Check if a Stop command has been received within the given duration.
async fn wait_for_stop(acq_rx: &mut rt::CommandReceiver<AcqCommand>, duration: Duration) -> bool {
    match rt::timeout(duration, acq_rx.recv()).await {
        Ok(Some(AcqCommand::Stop)) => true,
        Ok(Some(AcqCommand::Start)) => false,
        Ok(None) => true, // channel closed
        Err(_) => false,  // timeout
    }
}

/// Start the acquisition task thread.
pub(crate) fn start_acquisition_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
    rt::run_thread_named("MovingDotTask", move || acquisition_loop_async(ctx))
}

async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
    let mut rng = StdRng::from_os_rng();

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
            let mut img_data = physics::moving_dot_image_with_config(
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

            // Apply binning: sum bin_x * bin_y pixel blocks
            let bx = config.bin_x;
            let by = config.bin_y;
            let (out_data, out_w, out_h) = if bx > 1 || by > 1 {
                let bw = width / bx;
                let bh = height / by;
                let mut binned = vec![0.0f64; bw * bh];
                for row in 0..bh {
                    for col in 0..bw {
                        let mut sum = 0.0;
                        for dy in 0..by {
                            for dx in 0..bx {
                                sum += img_data[(row * by + dy) * width + col * bx + dx];
                            }
                        }
                        binned[row * bw + col] = sum;
                    }
                }
                (binned, bw, bh)
            } else {
                (img_data, width, height)
            };

            // Convert f64 photon counts to u16 (clamp to 0..65535)
            let u16_data: Vec<u16> = out_data
                .iter()
                .map(|&v| v.round().clamp(0.0, 65535.0) as u16)
                .collect();

            let dims = vec![NDDimension::new(out_w), NDDimension::new(out_h)];
            let mut frame = NDArray::with_data(dims, NDDataBuffer::U16(u16_data));

            // Exposure time delay with stop interruption
            let elapsed = start_time.elapsed().as_secs_f64();
            let delay = (config.acquire_time - elapsed).max(MIN_DELAY_SECS);
            if wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(delay)).await {
                ctx.end_acquisition(config.wait_for_plugins).await;
                break;
            }

            // Update counters
            num_counter += 1;
            array_counter += 1;

            frame.unique_id = array_counter;
            frame.timestamp = ad_core_rs::timestamp::EpicsTimestamp::now();

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
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_size_x,
                            addr: 0,
                            value: info.x_size as i32,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_size_y,
                            addr: 0,
                            value: info.y_size as i32,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_size,
                            addr: 0,
                            value: info.total_bytes as i32,
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
            if config.image_mode == ImageMode::Single
                || (config.image_mode == ImageMode::Multiple && num_counter >= config.num_images)
            {
                ctx.end_acquisition(config.wait_for_plugins).await;
                break;
            }

            // Period delay
            let total_elapsed = start_time.elapsed().as_secs_f64();
            let period_delay = config.acquire_period - total_elapsed;
            if period_delay > 0.0 {
                if wait_for_stop(&mut ctx.acq_rx, Duration::from_secs_f64(period_delay)).await {
                    ctx.end_acquisition(config.wait_for_plugins).await;
                    break;
                }
            }
        }
    }
}
