use std::sync::Arc;
use std::time::Duration;

use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{ADStatus, ImageMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::channel::{ArrayPublisher, QueuedArrayCounter};
use ad_core_rs::runtime as rt;

use crate::beamline_sim::{self, SimConfig};

use super::params::{XrtConfigSnapshot, XrtDetectorParams};
use super::types::DirtyFlags;

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
    pub xrt: XrtDetectorParams,
    pub sim_config: SimConfig,
    pub queued_counter: Arc<QueuedArrayCounter>,
}

impl AcquisitionContext {
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

pub(crate) fn start_acquisition_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
    rt::run_thread_named("XrtSimTask", move || acquisition_loop_async(ctx))
}

async fn acquisition_loop_async(mut ctx: AcquisitionContext) {
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
            match XrtConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.xrt).await {
                Ok(cfg) => cfg,
                Err(_) => continue,
            };

        loop {
            let start_time = std::time::Instant::now();

            // Take dirty flags
            let dirty_flags = ctx.dirty.lock().take();
            if dirty_flags.any {
                if let Ok(cfg) =
                    XrtConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.xrt).await
                {
                    config = cfg;
                }
            }

            // Accumulate ray tracing over acquire_time
            let mut stopped = false;
            let nx = ctx.sim_config.screen_nx;
            let nz = ctx.sim_config.screen_nz;
            let mut accum = vec![0.0f64; nx * nz];
            let mut total_captured = 0usize;
            let mut total_efficiency = 0.0f64;
            let mut last_cx = 0.0f64;
            let mut last_cz = 0.0f64;
            let mut last_fwhm_x = 0.0f64;
            let mut last_fwhm_z = 0.0f64;
            let mut last_rms_x = 0.0f64;
            let mut last_rms_z = 0.0f64;
            let mut last_source_energy = 0.0f64;
            let mut last_dcm_energy = 0.0f64;
            let mut n_iterations = 0u32;

            loop {
                // Check for stop
                match ctx.acq_rx.try_recv() {
                    Ok(AcqCommand::Stop) => {
                        ctx.end_acquisition(config.wait_for_plugins).await;
                        stopped = true;
                        break;
                    }
                    Ok(AcqCommand::Start) => {}
                    Err(rt::TryRecvError::Disconnected) => {
                        // Command channel closed = IOC shutdown; the asyn port is
                        // being torn down, so skip the idle writeback that would
                        // race the dead port and log "actor channel closed".
                        stopped = true;
                        break;
                    }
                    Err(rt::TryRecvError::Empty) => {}
                }

                // Check dirty flags for motor changes
                let dirty_flags = ctx.dirty.lock().take();
                if dirty_flags.any {
                    if let Ok(cfg) =
                        XrtConfigSnapshot::read_via_handle(&ctx.port_handle, &ctx.ad, &ctx.xrt)
                            .await
                    {
                        config = cfg;
                    }
                    // Reset accumulator on motor change
                    accum.fill(0.0);
                    total_captured = 0;
                    n_iterations = 0;
                }

                // Run one simulation iteration
                let sim_result = beamline_sim::simulate(&ctx.sim_config, &config.motors);

                // Accumulate intensity
                for (a, &v) in accum.iter_mut().zip(sim_result.capture.intensity.iter()) {
                    *a += v;
                }
                total_captured += sim_result.capture.n_captured;
                total_efficiency += sim_result.beamline_output.efficiency();
                n_iterations += 1;

                // Update stats from latest run
                let [cx, cz] = sim_result.capture.centroid();
                let [rms_x, rms_z] = sim_result.capture.rms_size();
                last_cx = cx;
                last_cz = cz;
                last_fwhm_x = sim_result.capture.fwhm_x().unwrap_or(0.0);
                last_fwhm_z = sim_result.capture.fwhm_z().unwrap_or(0.0);
                last_rms_x = rms_x;
                last_rms_z = rms_z;
                last_source_energy = sim_result.source_energy;
                last_dcm_energy = sim_result.dcm_energy;

                // Check if acquire_time reached
                if start_time.elapsed().as_secs_f64() >= config.acquire_time {
                    break;
                }
            }

            if stopped {
                break;
            }

            let efficiency = if n_iterations > 0 {
                total_efficiency / n_iterations as f64
            } else {
                0.0
            };
            let flux = accum.iter().sum::<f64>();
            let n_captured = total_captured;
            let cx = last_cx;
            let cz = last_cz;
            let fwhm_x = last_fwhm_x;
            let fwhm_z = last_fwhm_z;
            let rms_x = last_rms_x;
            let rms_z = last_rms_z;

            // Convert accumulated intensity to u16 image (transpose for AD)
            let max_val = accum.iter().cloned().fold(0.0_f64, f64::max);
            let scale = if max_val > 0.0 {
                65000.0 / max_val
            } else {
                0.0
            };

            let mut u16_data = vec![0u16; nx * nz];
            for ix in 0..nx {
                for iz in 0..nz {
                    let v = accum[ix * nz + iz] * scale;
                    u16_data[iz * nx + ix] = v.round().clamp(0.0, 65535.0) as u16;
                }
            }

            let dims = vec![NDDimension::new(nx), NDDimension::new(nz)];
            let mut frame = NDArray::with_data(dims, NDDataBuffer::U16(u16_data));

            // Update counters
            num_counter += 1;
            array_counter += 1;

            frame.unique_id = array_counter;
            frame.timestamp = ad_core_rs::timestamp::EpicsTimestamp::now();

            // Update simulation readback PVs + frame counters
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
                            value: nx as i32,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_size_y,
                            addr: 0,
                            value: nz as i32,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.ad.base.array_size,
                            addr: 0,
                            value: (nx * nz * 2) as i32,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.ad.base.timestamp_rbv,
                            addr: 0,
                            value: frame.timestamp.as_f64(),
                        },
                        // Simulation readbacks
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_source_energy,
                            addr: 0,
                            value: last_source_energy,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_dcm_energy,
                            addr: 0,
                            value: last_dcm_energy,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_efficiency,
                            addr: 0,
                            value: efficiency * 100.0,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_flux,
                            addr: 0,
                            value: flux,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_centroid_x,
                            addr: 0,
                            value: cx,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_centroid_z,
                            addr: 0,
                            value: cz,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_fwhm_x,
                            addr: 0,
                            value: fwhm_x,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_fwhm_z,
                            addr: 0,
                            value: fwhm_z,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_rms_x,
                            addr: 0,
                            value: rms_x,
                        },
                        asyn_rs::request::ParamSetValue::Float64 {
                            reason: ctx.xrt.sim_rms_z,
                            addr: 0,
                            value: rms_z,
                        },
                        asyn_rs::request::ParamSetValue::Int32 {
                            reason: ctx.xrt.sim_nrays,
                            addr: 0,
                            value: n_captured as i32,
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
