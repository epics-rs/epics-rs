use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::SeedableRng;
use rand::rngs::StdRng;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::{NDArrayOutput, QueuedArrayCounter};

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
    pub acq_rx: std::sync::mpsc::Receiver<AcqCommand>,
    pub port_handle: PortHandle,
    pub array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    pub dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    pub ad: ADBaseParams,
    pub dot: MovingDotParams,
    pub image_config: MovingDotImageConfig,
    pub queued_counter: Arc<QueuedArrayCounter>,
}

impl AcquisitionContext {
    /// Drain in-flight arrays (if configured) and transition to Idle.
    fn end_acquisition(&self, wait_for_plugins: bool) {
        if wait_for_plugins {
            self.queued_counter.wait_until_zero(Duration::from_secs(5));
        }
        self.port_handle
            .write_int32_no_wait(self.ad.acquire_busy, 0, 0);
        self.port_handle
            .write_int32_no_wait(self.ad.status, 0, ADStatus::Idle as i32);
        self.port_handle.write_int32_no_wait(self.ad.acquire, 0, 0);
        self.port_handle.call_param_callbacks_no_wait(0);
    }
}

/// Check if a Stop command has been received within the given duration.
fn wait_for_stop(acq_rx: &std::sync::mpsc::Receiver<AcqCommand>, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;

    if duration.as_millis() < 10 {
        loop {
            match acq_rx.try_recv() {
                Ok(AcqCommand::Stop) => return true,
                Ok(AcqCommand::Start) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::hint::spin_loop();
        }
    } else {
        match acq_rx.recv_timeout(duration) {
            Ok(AcqCommand::Stop) => true,
            Ok(AcqCommand::Start) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
        }
    }
}

/// Start the acquisition task thread.
pub(crate) fn start_acquisition_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("MovingDotTask".into())
        .spawn(move || acquisition_loop(ctx))
        .expect("failed to spawn MovingDotTask thread")
}

fn acquisition_loop(ctx: AcquisitionContext) {
    let mut rng = StdRng::from_os_rng();

    loop {
        // Wait for Start command
        match ctx.acq_rx.recv() {
            Ok(AcqCommand::Start) => {}
            Ok(AcqCommand::Stop) => continue,
            Err(_) => break,
        }

        // Initialize counters
        ctx.port_handle
            .write_int32_no_wait(ctx.ad.num_images_counter, 0, 0);
        ctx.port_handle
            .write_int32_no_wait(ctx.ad.status, 0, ADStatus::Acquire as i32);
        ctx.port_handle
            .write_int32_no_wait(ctx.ad.acquire_busy, 0, 1);

        let mut num_counter = 0;
        let mut array_counter = ctx
            .port_handle
            .read_int32_blocking(ctx.ad.base.array_counter, 0)
            .unwrap_or(0);

        // Read initial config
        let mut config =
            match MovingDotConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.dot) {
                Ok(cfg) => cfg,
                Err(_) => continue,
            };

        loop {
            let start_time = Instant::now();

            // Take dirty flags
            let dirty_flags = ctx.dirty.lock().take();
            let reset = dirty_flags.any;

            if reset {
                // If the read fails (e.g. port busy with CP link writes during motor move),
                // keep the previous config rather than aborting acquisition.
                if let Ok(cfg) =
                    MovingDotConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.dot)
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

            // Apply binning: sum bin_x × bin_y pixel blocks
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
            let mut frame = NDArray {
                unique_id: 0,
                timestamp: ad_core_rs::timestamp::EpicsTimestamp::default(),
                dims,
                data: NDDataBuffer::U16(u16_data),
                attributes: ad_core_rs::attributes::NDAttributeList::new(),
                codec: None,
            };

            // Exposure time delay with stop interruption
            let elapsed = start_time.elapsed().as_secs_f64();
            let delay = (config.acquire_time - elapsed).max(MIN_DELAY_SECS);
            if wait_for_stop(&ctx.acq_rx, Duration::from_secs_f64(delay)) {
                ctx.end_acquisition(config.wait_for_plugins);
                break;
            }

            // Update counters
            num_counter += 1;
            array_counter += 1;

            frame.unique_id = array_counter;
            frame.timestamp = ad_core_rs::timestamp::EpicsTimestamp::now();

            ctx.port_handle
                .write_int32_no_wait(ctx.ad.base.array_counter, 0, array_counter);
            ctx.port_handle
                .write_int32_no_wait(ctx.ad.num_images_counter, 0, num_counter);
            ctx.port_handle.write_float64_no_wait(
                ctx.ad.base.timestamp_rbv,
                0,
                frame.timestamp.as_f64(),
            );
            ctx.port_handle.write_int32_no_wait(
                ctx.ad.base.epics_ts_sec,
                0,
                frame.timestamp.sec as i32,
            );
            ctx.port_handle.write_int32_no_wait(
                ctx.ad.base.epics_ts_nsec,
                0,
                frame.timestamp.nsec as i32,
            );
            ctx.port_handle.call_param_callbacks_no_wait(0);

            if config.array_callbacks {
                ctx.array_output.lock().publish(Arc::new(frame));
            }

            // Check stop conditions
            if config.image_mode == ImageMode::Single
                || (config.image_mode == ImageMode::Multiple && num_counter >= config.num_images)
            {
                ctx.end_acquisition(config.wait_for_plugins);
                break;
            }

            // Period delay
            let total_elapsed = start_time.elapsed().as_secs_f64();
            let period_delay = config.acquire_period - total_elapsed;
            if period_delay > 0.0 {
                if wait_for_stop(&ctx.acq_rx, Duration::from_secs_f64(period_delay)) {
                    ctx.end_acquisition(config.wait_for_plugins);
                    break;
                }
            }
        }
    }
}
