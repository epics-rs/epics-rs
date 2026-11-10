use ad_core_rs::driver::{ColorMode, ImageMode};
use ad_core_rs::ndarray::NDDataType;
use ad_core_rs::params::ADBaseParams;
// ADBaseParams is now ADDriverParams; some fields moved to .base (NDArrayDriverParams)
use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::undefined_as_zero;

use crate::compute::{Gains, PeakParams, SineParams, SineWave};
use crate::driver::SimDetector;
use crate::types::{SimMode, SineOperation};

/// SimDetector-specific parameter indices.
#[derive(Clone, Copy)]
pub struct SimDetectorParams {
    pub gain_x: usize,
    pub gain_y: usize,
    pub gain_red: usize,
    pub gain_green: usize,
    pub gain_blue: usize,
    pub offset: usize,
    pub noise: usize,
    pub reset_image: usize,
    pub sim_mode: usize,
    pub peak_start_x: usize,
    pub peak_start_y: usize,
    pub peak_width_x: usize,
    pub peak_width_y: usize,
    pub peak_num_x: usize,
    pub peak_num_y: usize,
    pub peak_step_x: usize,
    pub peak_step_y: usize,
    pub peak_height_variation: usize,
    pub x_sine_operation: usize,
    pub x_sine1_amplitude: usize,
    pub x_sine1_frequency: usize,
    pub x_sine1_phase: usize,
    pub x_sine2_amplitude: usize,
    pub x_sine2_frequency: usize,
    pub x_sine2_phase: usize,
    pub y_sine_operation: usize,
    pub y_sine1_amplitude: usize,
    pub y_sine1_frequency: usize,
    pub y_sine1_phase: usize,
    pub y_sine2_amplitude: usize,
    pub y_sine2_frequency: usize,
    pub y_sine2_phase: usize,
}

impl SimDetectorParams {
    pub fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            gain_x: base.create_param("SIM_GAIN_X", ParamType::Float64)?,
            gain_y: base.create_param("SIM_GAIN_Y", ParamType::Float64)?,
            gain_red: base.create_param("SIM_GAIN_RED", ParamType::Float64)?,
            gain_green: base.create_param("SIM_GAIN_GREEN", ParamType::Float64)?,
            gain_blue: base.create_param("SIM_GAIN_BLUE", ParamType::Float64)?,
            offset: base.create_param("SIM_OFFSET", ParamType::Float64)?,
            noise: base.create_param("SIM_NOISE", ParamType::Float64)?,
            reset_image: base.create_param("RESET_IMAGE", ParamType::Int32)?,
            sim_mode: base.create_param("SIM_MODE", ParamType::Int32)?,
            peak_start_x: base.create_param("SIM_PEAK_START_X", ParamType::Int32)?,
            peak_start_y: base.create_param("SIM_PEAK_START_Y", ParamType::Int32)?,
            peak_width_x: base.create_param("SIM_PEAK_WIDTH_X", ParamType::Int32)?,
            peak_width_y: base.create_param("SIM_PEAK_WIDTH_Y", ParamType::Int32)?,
            peak_num_x: base.create_param("SIM_PEAK_NUM_X", ParamType::Int32)?,
            peak_num_y: base.create_param("SIM_PEAK_NUM_Y", ParamType::Int32)?,
            peak_step_x: base.create_param("SIM_PEAK_STEP_X", ParamType::Int32)?,
            peak_step_y: base.create_param("SIM_PEAK_STEP_Y", ParamType::Int32)?,
            peak_height_variation: base
                .create_param("SIM_PEAK_HEIGHT_VARIATION", ParamType::Float64)?,
            x_sine_operation: base.create_param("SIM_XSINE_OPERATION", ParamType::Int32)?,
            x_sine1_amplitude: base.create_param("SIM_XSINE1_AMPLITUDE", ParamType::Float64)?,
            x_sine1_frequency: base.create_param("SIM_XSINE1_FREQUENCY", ParamType::Float64)?,
            x_sine1_phase: base.create_param("SIM_XSINE1_PHASE", ParamType::Float64)?,
            x_sine2_amplitude: base.create_param("SIM_XSINE2_AMPLITUDE", ParamType::Float64)?,
            x_sine2_frequency: base.create_param("SIM_XSINE2_FREQUENCY", ParamType::Float64)?,
            x_sine2_phase: base.create_param("SIM_XSINE2_PHASE", ParamType::Float64)?,
            y_sine_operation: base.create_param("SIM_YSINE_OPERATION", ParamType::Int32)?,
            y_sine1_amplitude: base.create_param("SIM_YSINE1_AMPLITUDE", ParamType::Float64)?,
            y_sine1_frequency: base.create_param("SIM_YSINE1_FREQUENCY", ParamType::Float64)?,
            y_sine1_phase: base.create_param("SIM_YSINE1_PHASE", ParamType::Float64)?,
            y_sine2_amplitude: base.create_param("SIM_YSINE2_AMPLITUDE", ParamType::Float64)?,
            y_sine2_frequency: base.create_param("SIM_YSINE2_FREQUENCY", ParamType::Float64)?,
            y_sine2_phase: base.create_param("SIM_YSINE2_PHASE", ParamType::Float64)?,
        })
    }
}

/// Snapshot of all configuration needed for one frame computation.
/// Read once under lock, used without lock.
pub struct SimConfigSnapshot {
    pub sim_mode: SimMode,
    pub gains: Gains,
    pub peak: PeakParams,
    pub sine: SineParams,
    pub offset: f64,
    pub noise: f64,
    pub data_type: NDDataType,
    pub color_mode: ColorMode,
    pub max_size_x: usize,
    pub max_size_y: usize,
    pub size_x: usize,
    pub size_y: usize,
    pub min_x: usize,
    pub min_y: usize,
    pub acquire_time: f64,
    pub acquire_period: f64,
    pub image_mode: ImageMode,
    pub num_images: i32,
    pub array_callbacks: bool,
    pub wait_for_plugins: bool,
}

/// The layout C `simDetector::computeImage` draws for `NDColorMode`
/// (simDetector.cpp:599-623): Mono, RGB1, RGB2 or RGB3. It has no case for
/// Bayer or YUV, which `simDetector.template` does not offer; those are drawn
/// as Mono.
fn sim_color_mode(value: i32) -> ColorMode {
    match ColorMode::from_i32(value) {
        mode @ (ColorMode::RGB1 | ColorMode::RGB2 | ColorMode::RGB3) => mode,
        _ => ColorMode::Mono,
    }
}

impl SimConfigSnapshot {
    /// [`Self::read_from`] under one hold of the port, as C `simTask` reads
    /// its whole config with `getIntegerParam` between one `lock()` and
    /// `unlock()`; a round trip per parameter kept Acquire busy for each one.
    pub async fn read_via_handle(handle: &PortHandle) -> AsynResult<Self> {
        handle
            .with_driver(|d: &mut SimDetector| {
                Self::read_from(&d.ad.port_base, &d.ad.params, &d.sim_params)
            })
            .await?
    }

    pub fn read_from(
        base: &PortDriverBase,
        ad: &ADBaseParams,
        sim: &SimDetectorParams,
    ) -> AsynResult<Self> {
        let dt_ord = undefined_as_zero(base.get_int32_param(ad.base.data_type, 0))? as u8;
        let data_type = NDDataType::from_ordinal(dt_ord).unwrap_or(NDDataType::UInt8);

        let cm = undefined_as_zero(base.get_int32_param(ad.base.color_mode, 0))?;
        let color_mode = sim_color_mode(cm);

        Ok(Self {
            sim_mode: SimMode::from_i32(undefined_as_zero(base.get_int32_param(sim.sim_mode, 0))?),
            gains: Gains {
                gain: undefined_as_zero(base.get_float64_param(ad.gain, 0))?,
                gain_x: undefined_as_zero(base.get_float64_param(sim.gain_x, 0))?,
                gain_y: undefined_as_zero(base.get_float64_param(sim.gain_y, 0))?,
                gain_red: undefined_as_zero(base.get_float64_param(sim.gain_red, 0))?,
                gain_green: undefined_as_zero(base.get_float64_param(sim.gain_green, 0))?,
                gain_blue: undefined_as_zero(base.get_float64_param(sim.gain_blue, 0))?,
            },
            peak: PeakParams {
                start_x: undefined_as_zero(base.get_int32_param(sim.peak_start_x, 0))?,
                start_y: undefined_as_zero(base.get_int32_param(sim.peak_start_y, 0))?,
                width_x: undefined_as_zero(base.get_int32_param(sim.peak_width_x, 0))?,
                width_y: undefined_as_zero(base.get_int32_param(sim.peak_width_y, 0))?,
                num_x: undefined_as_zero(base.get_int32_param(sim.peak_num_x, 0))?,
                num_y: undefined_as_zero(base.get_int32_param(sim.peak_num_y, 0))?,
                step_x: undefined_as_zero(base.get_int32_param(sim.peak_step_x, 0))?,
                step_y: undefined_as_zero(base.get_int32_param(sim.peak_step_y, 0))?,
                height_variation: undefined_as_zero(
                    base.get_float64_param(sim.peak_height_variation, 0),
                )?,
            },
            sine: SineParams {
                x_sine1: SineWave {
                    amplitude: undefined_as_zero(base.get_float64_param(sim.x_sine1_amplitude, 0))?,
                    frequency: undefined_as_zero(base.get_float64_param(sim.x_sine1_frequency, 0))?,
                    phase: undefined_as_zero(base.get_float64_param(sim.x_sine1_phase, 0))?,
                },
                x_sine2: SineWave {
                    amplitude: undefined_as_zero(base.get_float64_param(sim.x_sine2_amplitude, 0))?,
                    frequency: undefined_as_zero(base.get_float64_param(sim.x_sine2_frequency, 0))?,
                    phase: undefined_as_zero(base.get_float64_param(sim.x_sine2_phase, 0))?,
                },
                y_sine1: SineWave {
                    amplitude: undefined_as_zero(base.get_float64_param(sim.y_sine1_amplitude, 0))?,
                    frequency: undefined_as_zero(base.get_float64_param(sim.y_sine1_frequency, 0))?,
                    phase: undefined_as_zero(base.get_float64_param(sim.y_sine1_phase, 0))?,
                },
                y_sine2: SineWave {
                    amplitude: undefined_as_zero(base.get_float64_param(sim.y_sine2_amplitude, 0))?,
                    frequency: undefined_as_zero(base.get_float64_param(sim.y_sine2_frequency, 0))?,
                    phase: undefined_as_zero(base.get_float64_param(sim.y_sine2_phase, 0))?,
                },
                x_op: SineOperation::from_i32(undefined_as_zero(
                    base.get_int32_param(sim.x_sine_operation, 0),
                )?),
                y_op: SineOperation::from_i32(undefined_as_zero(
                    base.get_int32_param(sim.y_sine_operation, 0),
                )?),
            },
            offset: undefined_as_zero(base.get_float64_param(sim.offset, 0))?,
            noise: undefined_as_zero(base.get_float64_param(sim.noise, 0))?,
            data_type,
            color_mode,
            max_size_x: undefined_as_zero(base.get_int32_param(ad.max_size_x, 0))? as usize,
            max_size_y: undefined_as_zero(base.get_int32_param(ad.max_size_y, 0))? as usize,
            size_x: undefined_as_zero(base.get_int32_param(ad.size_x, 0))? as usize,
            size_y: undefined_as_zero(base.get_int32_param(ad.size_y, 0))? as usize,
            min_x: undefined_as_zero(base.get_int32_param(ad.min_x, 0))? as usize,
            min_y: undefined_as_zero(base.get_int32_param(ad.min_y, 0))? as usize,
            acquire_time: undefined_as_zero(base.get_float64_param(ad.acquire_time, 0))?,
            acquire_period: undefined_as_zero(base.get_float64_param(ad.acquire_period, 0))?,
            image_mode: ImageMode::from_i32(undefined_as_zero(
                base.get_int32_param(ad.image_mode, 0),
            )?),
            num_images: undefined_as_zero(base.get_int32_param(ad.num_images, 0))?,
            array_callbacks: undefined_as_zero(base.get_int32_param(ad.base.array_callbacks, 0))?
                != 0,
            wait_for_plugins: undefined_as_zero(base.get_int32_param(ad.base.wait_for_plugins, 0))?
                != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::InterfaceMask;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    #[test]
    fn test_create_params() {
        let mut base = PortDriverBase::new(
            "test",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let params = SimDetectorParams::create(&mut base).unwrap();
        assert!(base.find_param("SIM_GAIN_X").is_some());
        assert!(base.find_param("SIM_MODE").is_some());
        assert!(base.find_param("SIM_PEAK_WIDTH_X").is_some());
        assert!(base.find_param("SIM_YSINE2_PHASE").is_some());
        assert_eq!(params.gain_x, base.find_param("SIM_GAIN_X").unwrap());
    }

    /// C `simTask` reads its config between one `lock()` and `unlock()`, so a
    /// write of two parameters under one lock is seen whole or not at all.
    #[test]
    fn a_config_read_never_sees_half_of_one_locked_write() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let rt = crate::driver::create_sim_detector(
            &asyn_rs::registry::claim_port("SIM_CFG_ONE_LOCK").unwrap(),
            64,
            64,
            NDDataType::UInt8 as i32,
            1_000_000,
            ad_core_rs::plugin::channel::NDArrayOutput::new(),
        )
        .unwrap();
        let port = rt.port_handle().clone();
        let (size_x, size_y) = (rt.ad_params.size_x, rt.ad_params.size_y);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (port, stop) = (port.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut k = 1;
                while !stop.load(Ordering::Relaxed) {
                    port.with_driver_blocking(move |d: &mut SimDetector| {
                        d.ad.port_base.set_int32_param(size_x, 0, k).unwrap();
                        d.ad.port_base.set_int32_param(size_y, 0, k).unwrap();
                    })
                    .unwrap();
                    k = k % 64 + 1;
                }
            })
        };
        for _ in 0..200 {
            let config = epics_libcom_rs::runtime::task::block_on_sync(
                SimConfigSnapshot::read_via_handle(&port),
            )
            .unwrap()
            .unwrap();
            assert_eq!(config.size_x, config.size_y);
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
