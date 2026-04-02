use ad_core_rs::driver::{ColorMode, ImageMode};
use ad_core_rs::ndarray::NDDataType;
use ad_core_rs::params::ADBaseParams;
// ADBaseParams is now ADDriverParams; some fields moved to .base (NDArrayDriverParams)
use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use crate::compute::{Gains, PeakParams, SineParams, SineWave};
use crate::types::{SimMode, SineOperation};

/// SimDetector-specific parameter indices.
#[derive(Clone, Copy)]
pub struct SimDetectorParams {
    pub gain: usize,
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
            gain: base.create_param("AD_GAIN", ParamType::Float64)?,
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

impl SimConfigSnapshot {
    /// Read config via PortHandle (blocking). For use from the acquisition task thread.
    pub fn read_via_handle(
        handle: &PortHandle,
        ad: &ADBaseParams,
        sim: &SimDetectorParams,
    ) -> AsynResult<Self> {
        let dt_ord = handle.read_int32_blocking(ad.base.data_type, 0)? as u8;
        let data_type = NDDataType::from_ordinal(dt_ord).unwrap_or(NDDataType::UInt8);

        let cm = handle.read_int32_blocking(ad.base.color_mode, 0)?;
        let color_mode = match cm {
            2 => ColorMode::RGB1,
            _ => ColorMode::Mono,
        };

        Ok(Self {
            sim_mode: SimMode::from_i32(handle.read_int32_blocking(sim.sim_mode, 0)?),
            gains: Gains {
                gain: handle.read_float64_blocking(sim.gain, 0)?,
                gain_x: handle.read_float64_blocking(sim.gain_x, 0)?,
                gain_y: handle.read_float64_blocking(sim.gain_y, 0)?,
                gain_red: handle.read_float64_blocking(sim.gain_red, 0)?,
                gain_green: handle.read_float64_blocking(sim.gain_green, 0)?,
                gain_blue: handle.read_float64_blocking(sim.gain_blue, 0)?,
            },
            peak: PeakParams {
                start_x: handle.read_int32_blocking(sim.peak_start_x, 0)?,
                start_y: handle.read_int32_blocking(sim.peak_start_y, 0)?,
                width_x: handle.read_int32_blocking(sim.peak_width_x, 0)?,
                width_y: handle.read_int32_blocking(sim.peak_width_y, 0)?,
                num_x: handle.read_int32_blocking(sim.peak_num_x, 0)?,
                num_y: handle.read_int32_blocking(sim.peak_num_y, 0)?,
                step_x: handle.read_int32_blocking(sim.peak_step_x, 0)?,
                step_y: handle.read_int32_blocking(sim.peak_step_y, 0)?,
                height_variation: handle.read_float64_blocking(sim.peak_height_variation, 0)?,
            },
            sine: SineParams {
                x_sine1: SineWave {
                    amplitude: handle.read_float64_blocking(sim.x_sine1_amplitude, 0)?,
                    frequency: handle.read_float64_blocking(sim.x_sine1_frequency, 0)?,
                    phase: handle.read_float64_blocking(sim.x_sine1_phase, 0)?,
                },
                x_sine2: SineWave {
                    amplitude: handle.read_float64_blocking(sim.x_sine2_amplitude, 0)?,
                    frequency: handle.read_float64_blocking(sim.x_sine2_frequency, 0)?,
                    phase: handle.read_float64_blocking(sim.x_sine2_phase, 0)?,
                },
                y_sine1: SineWave {
                    amplitude: handle.read_float64_blocking(sim.y_sine1_amplitude, 0)?,
                    frequency: handle.read_float64_blocking(sim.y_sine1_frequency, 0)?,
                    phase: handle.read_float64_blocking(sim.y_sine1_phase, 0)?,
                },
                y_sine2: SineWave {
                    amplitude: handle.read_float64_blocking(sim.y_sine2_amplitude, 0)?,
                    frequency: handle.read_float64_blocking(sim.y_sine2_frequency, 0)?,
                    phase: handle.read_float64_blocking(sim.y_sine2_phase, 0)?,
                },
                x_op: SineOperation::from_i32(handle.read_int32_blocking(sim.x_sine_operation, 0)?),
                y_op: SineOperation::from_i32(handle.read_int32_blocking(sim.y_sine_operation, 0)?),
            },
            offset: handle.read_float64_blocking(sim.offset, 0)?,
            noise: handle.read_float64_blocking(sim.noise, 0)?,
            data_type,
            color_mode,
            max_size_x: handle.read_int32_blocking(ad.max_size_x, 0)? as usize,
            max_size_y: handle.read_int32_blocking(ad.max_size_y, 0)? as usize,
            size_x: handle.read_int32_blocking(ad.size_x, 0)? as usize,
            size_y: handle.read_int32_blocking(ad.size_y, 0)? as usize,
            min_x: handle.read_int32_blocking(ad.min_x, 0).unwrap_or(0) as usize,
            min_y: handle.read_int32_blocking(ad.min_y, 0).unwrap_or(0) as usize,
            acquire_time: handle.read_float64_blocking(ad.acquire_time, 0)?,
            acquire_period: handle.read_float64_blocking(ad.acquire_period, 0)?,
            image_mode: ImageMode::from_i32(handle.read_int32_blocking(ad.image_mode, 0)?),
            num_images: handle.read_int32_blocking(ad.num_images, 0)?,
            array_callbacks: handle.read_int32_blocking(ad.base.array_callbacks, 0)? != 0,
            wait_for_plugins: handle
                .read_int32_blocking(ad.base.wait_for_plugins, 0)
                .unwrap_or(0)
                != 0,
        })
    }

    pub fn read_from(
        base: &PortDriverBase,
        ad: &ADBaseParams,
        sim: &SimDetectorParams,
    ) -> AsynResult<Self> {
        let dt_ord = base.get_int32_param(ad.base.data_type, 0)? as u8;
        let data_type = NDDataType::from_ordinal(dt_ord).unwrap_or(NDDataType::UInt8);

        let cm = base.get_int32_param(ad.base.color_mode, 0)?;
        let color_mode = match cm {
            2 => ColorMode::RGB1,
            _ => ColorMode::Mono,
        };

        Ok(Self {
            sim_mode: SimMode::from_i32(base.get_int32_param(sim.sim_mode, 0)?),
            gains: Gains {
                gain: base.get_float64_param(sim.gain, 0)?,
                gain_x: base.get_float64_param(sim.gain_x, 0)?,
                gain_y: base.get_float64_param(sim.gain_y, 0)?,
                gain_red: base.get_float64_param(sim.gain_red, 0)?,
                gain_green: base.get_float64_param(sim.gain_green, 0)?,
                gain_blue: base.get_float64_param(sim.gain_blue, 0)?,
            },
            peak: PeakParams {
                start_x: base.get_int32_param(sim.peak_start_x, 0)?,
                start_y: base.get_int32_param(sim.peak_start_y, 0)?,
                width_x: base.get_int32_param(sim.peak_width_x, 0)?,
                width_y: base.get_int32_param(sim.peak_width_y, 0)?,
                num_x: base.get_int32_param(sim.peak_num_x, 0)?,
                num_y: base.get_int32_param(sim.peak_num_y, 0)?,
                step_x: base.get_int32_param(sim.peak_step_x, 0)?,
                step_y: base.get_int32_param(sim.peak_step_y, 0)?,
                height_variation: base.get_float64_param(sim.peak_height_variation, 0)?,
            },
            sine: SineParams {
                x_sine1: SineWave {
                    amplitude: base.get_float64_param(sim.x_sine1_amplitude, 0)?,
                    frequency: base.get_float64_param(sim.x_sine1_frequency, 0)?,
                    phase: base.get_float64_param(sim.x_sine1_phase, 0)?,
                },
                x_sine2: SineWave {
                    amplitude: base.get_float64_param(sim.x_sine2_amplitude, 0)?,
                    frequency: base.get_float64_param(sim.x_sine2_frequency, 0)?,
                    phase: base.get_float64_param(sim.x_sine2_phase, 0)?,
                },
                y_sine1: SineWave {
                    amplitude: base.get_float64_param(sim.y_sine1_amplitude, 0)?,
                    frequency: base.get_float64_param(sim.y_sine1_frequency, 0)?,
                    phase: base.get_float64_param(sim.y_sine1_phase, 0)?,
                },
                y_sine2: SineWave {
                    amplitude: base.get_float64_param(sim.y_sine2_amplitude, 0)?,
                    frequency: base.get_float64_param(sim.y_sine2_frequency, 0)?,
                    phase: base.get_float64_param(sim.y_sine2_phase, 0)?,
                },
                x_op: SineOperation::from_i32(base.get_int32_param(sim.x_sine_operation, 0)?),
                y_op: SineOperation::from_i32(base.get_int32_param(sim.y_sine_operation, 0)?),
            },
            offset: base.get_float64_param(sim.offset, 0)?,
            noise: base.get_float64_param(sim.noise, 0)?,
            data_type,
            color_mode,
            max_size_x: base.get_int32_param(ad.max_size_x, 0)? as usize,
            max_size_y: base.get_int32_param(ad.max_size_y, 0)? as usize,
            size_x: base.get_int32_param(ad.size_x, 0)? as usize,
            size_y: base.get_int32_param(ad.size_y, 0)? as usize,
            min_x: base.get_int32_param(ad.min_x, 0).unwrap_or(0) as usize,
            min_y: base.get_int32_param(ad.min_y, 0).unwrap_or(0) as usize,
            acquire_time: base.get_float64_param(ad.acquire_time, 0)?,
            acquire_period: base.get_float64_param(ad.acquire_period, 0)?,
            image_mode: ImageMode::from_i32(base.get_int32_param(ad.image_mode, 0)?),
            num_images: base.get_int32_param(ad.num_images, 0)?,
            array_callbacks: base.get_int32_param(ad.base.array_callbacks, 0)? != 0,
            wait_for_plugins: base
                .get_int32_param(ad.base.wait_for_plugins, 0)
                .unwrap_or(0)
                != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    #[test]
    fn test_create_params() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let params = SimDetectorParams::create(&mut base).unwrap();
        assert!(base.find_param("SIM_GAIN_X").is_some());
        assert!(base.find_param("SIM_MODE").is_some());
        assert!(base.find_param("SIM_PEAK_WIDTH_X").is_some());
        assert!(base.find_param("SIM_YSINE2_PHASE").is_some());
        assert_eq!(params.gain_x, base.find_param("SIM_GAIN_X").unwrap());
    }
}
