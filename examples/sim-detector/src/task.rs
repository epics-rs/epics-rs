use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::SeedableRng;

use asyn_rs::port_handle::PortHandle;

use ad_core::driver::{ADStatus, ImageMode};
use ad_core::ndarray::{NDArray, NDDataBuffer};
use ad_core::params::ADBaseParams;
use ad_core::plugin::channel::NDArrayOutput;

use ad_core::color_layout::ColorLayout;
use ad_core::roi::crop_roi;

use crate::compute::{self, SineState};
use crate::params::{SimConfigSnapshot, SimDetectorParams};
use crate::types::DirtyFlags;

const MIN_DELAY_SECS: f64 = 1e-5;

/// Commands sent from the driver to the acquisition task.
pub enum AcqCommand {
    Start,
    Stop,
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
            rng: StdRng::from_entropy(),
            raw_buf: NDDataBuffer::zeros(ad_core::ndarray::NDDataType::UInt8, 0),
            background_buf: NDDataBuffer::zeros(ad_core::ndarray::NDDataType::UInt8, 0),
            ramp_buf: NDDataBuffer::zeros(ad_core::ndarray::NDDataType::UInt8, 0),
            peak_buf: NDDataBuffer::zeros(ad_core::ndarray::NDDataType::UInt8, 0),
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

        crop_roi(&self.raw_buf, &layout, min_x, min_y, size_x, size_y)
    }
}

/// Check if a Stop command has been received within the given duration.
/// Uses spin + try_recv for short delays to avoid macOS recv_timeout oversleep.
fn wait_for_stop(acq_rx: &std::sync::mpsc::Receiver<AcqCommand>, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;

    if duration.as_millis() < 10 {
        // Short delay: spin with try_recv to avoid macOS timer resolution issues
        loop {
            match acq_rx.try_recv() {
                Ok(AcqCommand::Stop) => return true,
                Ok(AcqCommand::Start) => {} // stale start, ignore
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::hint::spin_loop();
        }
    } else {
        // Longer delay: use recv_timeout
        match acq_rx.recv_timeout(duration) {
            Ok(AcqCommand::Stop) => true,
            Ok(AcqCommand::Start) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
        }
    }
}

/// Start the acquisition task thread.
///
/// The task reads config via `PortHandle` (blocking), computes frames,
/// and publishes via `NDArrayOutput`. Dirty flags are shared with the driver
/// via `Arc<parking_lot::Mutex<DirtyFlags>>`.
pub fn start_acquisition_task(
    acq_rx: std::sync::mpsc::Receiver<AcqCommand>,
    port_handle: PortHandle,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    ad_params: ADBaseParams,
    sim_params: SimDetectorParams,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("SimDetTask".into())
        .spawn(move || {
            acquisition_loop(acq_rx, port_handle, array_output, dirty, ad_params, sim_params);
        })
        .expect("failed to spawn SimDetTask thread")
}

fn acquisition_loop(
    acq_rx: std::sync::mpsc::Receiver<AcqCommand>,
    port_handle: PortHandle,
    array_output: Arc<parking_lot::Mutex<NDArrayOutput>>,
    dirty: Arc<parking_lot::Mutex<DirtyFlags>>,
    ad: ADBaseParams,
    sim: SimDetectorParams,
) {
    let mut task_state = TaskState::new();

    loop {
        // Wait for Start command
        match acq_rx.recv() {
            Ok(AcqCommand::Start) => {}
            Ok(AcqCommand::Stop) => continue, // stale stop, ignore
            Err(_) => break,                  // channel closed = shutdown
        }

        // Initialize counters via PortHandle
        let _ = port_handle.write_int32_blocking(ad.num_images_counter, 0, 0);
        let _ = port_handle.write_int32_blocking(ad.status, 0, ADStatus::Acquire as i32);
        let _ = port_handle.write_int32_blocking(ad.acquire_busy, 0, 1);

        let mut num_counter = 0;
        let mut array_counter = port_handle.read_int32_blocking(ad.base.array_counter, 0).unwrap_or(0);

        // Read initial config
        let mut config = match SimConfigSnapshot::read_via_handle(&port_handle, &ad, &sim) {
            Ok(cfg) => cfg,
            Err(_) => continue,
        };

        loop {
            let start_time = Instant::now();

            // Take dirty flags (shared with driver)
            let dirty_flags = dirty.lock().take();
            let reset = dirty_flags.any();

            // Only re-read config when parameters changed
            if reset {
                config = match SimConfigSnapshot::read_via_handle(&port_handle, &ad, &sim) {
                    Ok(cfg) => cfg,
                    Err(_) => break,
                };
            }
            task_state.apply_dirty(&dirty_flags, &config);

            let mut frame = task_state.compute_frame(&config, reset);

            // Exposure time sleep with stop interruption
            let elapsed = start_time.elapsed().as_secs_f64();
            let delay = (config.acquire_time - elapsed).max(MIN_DELAY_SECS);
            if wait_for_stop(&acq_rx, Duration::from_secs_f64(delay)) {
                // External stop received
                let _ = port_handle.write_int32_blocking(ad.acquire_busy, 0, 0);
                let _ = port_handle.write_int32_blocking(ad.status, 0, ADStatus::Idle as i32);
                let _ = port_handle.write_int32_blocking(ad.acquire, 0, 0);
                let _ = port_handle.call_param_callbacks_blocking(0);
                break;
            }
            // Update counters (local to avoid blocking round-trips)
            num_counter += 1;
            array_counter += 1;

            frame.unique_id = array_counter;
            frame.timestamp = ad_core::timestamp::EpicsTimestamp::now();

            // Counter updates + callParamCallbacks always run (like C EPICS).
            // Only doCallbacksGenericPointer (publish) is gated by array_callbacks.
            port_handle.write_int32_no_wait(ad.base.array_counter, 0, array_counter);
            port_handle.write_int32_no_wait(ad.num_images_counter, 0, num_counter);
            port_handle.write_float64_no_wait(ad.base.timestamp_rbv, 0, frame.timestamp.as_f64());
            port_handle.write_int32_no_wait(ad.base.epics_ts_sec, 0, frame.timestamp.sec as i32);
            port_handle.write_int32_no_wait(ad.base.epics_ts_nsec, 0, frame.timestamp.nsec as i32);
            let _ = port_handle.call_param_callbacks_blocking(0);

            if config.array_callbacks {
                let output = array_output.lock();
                if config.wait_for_plugins {
                    output.publish_and_wait(Arc::new(frame));
                } else {
                    output.publish(Arc::new(frame));
                }
            }

            // Check stop conditions
            let image_mode = config.image_mode;
            let num_images = config.num_images;
            if image_mode == ImageMode::Single
                || (image_mode == ImageMode::Multiple && num_counter >= num_images)
            {
                let _ = port_handle.write_int32_blocking(ad.acquire_busy, 0, 0);
                let _ = port_handle.write_int32_blocking(ad.status, 0, ADStatus::Idle as i32);
                let _ = port_handle.write_int32_blocking(ad.acquire, 0, 0);
                let _ = port_handle.call_param_callbacks_blocking(0);
                break;
            }

            // Period delay with stop interruption
            let total_elapsed = start_time.elapsed().as_secs_f64();
            let period_delay = config.acquire_period - total_elapsed;
            if period_delay > 0.0 {
                if wait_for_stop(&acq_rx, Duration::from_secs_f64(period_delay)) {
                    let _ = port_handle.write_int32_blocking(ad.acquire_busy, 0, 0);
                    let _ = port_handle.write_int32_blocking(ad.status, 0, ADStatus::Idle as i32);
                    let _ = port_handle.write_int32_blocking(ad.acquire, 0, 0);
                    let _ = port_handle.call_param_callbacks_blocking(0);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::create_sim_detector;
    use ad_core::plugin::channel::NDArrayOutput;

    #[test]
    fn test_single_mode_auto_stop() {
        let rt = create_sim_detector("SIM_TEST", 32, 32, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();

        handle.write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_period, 0, 0.001).unwrap();

        // Start acquisition
        handle.write_int32_blocking(rt.ad_params.acquire, 0, 1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(200));

        let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
        assert_eq!(acquire, 0, "acquire should be 0 after Single mode completes");
        let counter = handle.read_int32_blocking(rt.ad_params.base.array_counter, 0).unwrap();
        assert!(counter >= 1, "should have produced at least 1 frame");
    }

    #[test]
    fn test_continuous_mode_produces_frames() {
        let rt = create_sim_detector("SIM_CONT", 16, 16, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();

        handle.write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_period, 0, 0.002).unwrap();

        // Start acquisition
        handle.write_int32_blocking(rt.ad_params.acquire, 0, 1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Stop acquisition
        handle.write_int32_blocking(rt.ad_params.acquire, 0, 0).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let counter = handle.read_int32_blocking(rt.ad_params.base.array_counter, 0).unwrap();
        assert!(counter >= 2, "should have produced multiple frames, got {}", counter);
    }

    #[test]
    fn test_stop_during_acquisition() {
        let rt = create_sim_detector("SIM_STOP", 8, 8, 1_000_000, NDArrayOutput::new()).unwrap();
        let handle = rt.port_handle();

        handle.write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_time, 0, 0.5).unwrap();
        handle.write_float64_blocking(rt.ad_params.acquire_period, 0, 1.0).unwrap();

        // Start acquisition
        handle.write_int32_blocking(rt.ad_params.acquire, 0, 1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        // Stop during long exposure
        handle.write_int32_blocking(rt.ad_params.acquire, 0, 0).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
        assert_eq!(acquire, 0);
    }
}
