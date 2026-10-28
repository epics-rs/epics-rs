use ad_core_rs::driver::{ColorMode, ImageMode};
use ad_core_rs::ndarray::NDDataType;
use ad_core_rs::params::ADBaseParams;
// ADBaseParams is now ADDriverParams; some fields moved to .base (NDArrayDriverParams)
use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{snapshot_float64, snapshot_int32, undefined_as_zero};

use crate::compute::{Gains, PeakParams, SineParams, SineWave};
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
    /// Read config via PortHandle (async). For use from the acquisition task thread.
    pub async fn read_via_handle(
        handle: &PortHandle,
        ad: &ADBaseParams,
        sim: &SimDetectorParams,
    ) -> AsynResult<Self> {
        let dt_ord = snapshot_int32(handle, ad.base.data_type, 0).await? as u8;
        let data_type = NDDataType::from_ordinal(dt_ord).unwrap_or(NDDataType::UInt8);

        let cm = snapshot_int32(handle, ad.base.color_mode, 0).await?;
        let color_mode = sim_color_mode(cm);

        Ok(Self {
            sim_mode: SimMode::from_i32(snapshot_int32(handle, sim.sim_mode, 0).await?),
            gains: Gains {
                gain: snapshot_float64(handle, ad.gain, 0).await?,
                gain_x: snapshot_float64(handle, sim.gain_x, 0).await?,
                gain_y: snapshot_float64(handle, sim.gain_y, 0).await?,
                gain_red: snapshot_float64(handle, sim.gain_red, 0).await?,
                gain_green: snapshot_float64(handle, sim.gain_green, 0).await?,
                gain_blue: snapshot_float64(handle, sim.gain_blue, 0).await?,
            },
            peak: PeakParams {
                start_x: snapshot_int32(handle, sim.peak_start_x, 0).await?,
                start_y: snapshot_int32(handle, sim.peak_start_y, 0).await?,
                width_x: snapshot_int32(handle, sim.peak_width_x, 0).await?,
                width_y: snapshot_int32(handle, sim.peak_width_y, 0).await?,
                num_x: snapshot_int32(handle, sim.peak_num_x, 0).await?,
                num_y: snapshot_int32(handle, sim.peak_num_y, 0).await?,
                step_x: snapshot_int32(handle, sim.peak_step_x, 0).await?,
                step_y: snapshot_int32(handle, sim.peak_step_y, 0).await?,
                height_variation: snapshot_float64(handle, sim.peak_height_variation, 0).await?,
            },
            sine: SineParams {
                x_sine1: SineWave {
                    amplitude: snapshot_float64(handle, sim.x_sine1_amplitude, 0).await?,
                    frequency: snapshot_float64(handle, sim.x_sine1_frequency, 0).await?,
                    phase: snapshot_float64(handle, sim.x_sine1_phase, 0).await?,
                },
                x_sine2: SineWave {
                    amplitude: snapshot_float64(handle, sim.x_sine2_amplitude, 0).await?,
                    frequency: snapshot_float64(handle, sim.x_sine2_frequency, 0).await?,
                    phase: snapshot_float64(handle, sim.x_sine2_phase, 0).await?,
                },
                y_sine1: SineWave {
                    amplitude: snapshot_float64(handle, sim.y_sine1_amplitude, 0).await?,
                    frequency: snapshot_float64(handle, sim.y_sine1_frequency, 0).await?,
                    phase: snapshot_float64(handle, sim.y_sine1_phase, 0).await?,
                },
                y_sine2: SineWave {
                    amplitude: snapshot_float64(handle, sim.y_sine2_amplitude, 0).await?,
                    frequency: snapshot_float64(handle, sim.y_sine2_frequency, 0).await?,
                    phase: snapshot_float64(handle, sim.y_sine2_phase, 0).await?,
                },
                x_op: SineOperation::from_i32(
                    snapshot_int32(handle, sim.x_sine_operation, 0).await?,
                ),
                y_op: SineOperation::from_i32(
                    snapshot_int32(handle, sim.y_sine_operation, 0).await?,
                ),
            },
            offset: snapshot_float64(handle, sim.offset, 0).await?,
            noise: snapshot_float64(handle, sim.noise, 0).await?,
            data_type,
            color_mode,
            max_size_x: snapshot_int32(handle, ad.max_size_x, 0).await? as usize,
            max_size_y: snapshot_int32(handle, ad.max_size_y, 0).await? as usize,
            size_x: snapshot_int32(handle, ad.size_x, 0).await? as usize,
            size_y: snapshot_int32(handle, ad.size_y, 0).await? as usize,
            min_x: snapshot_int32(handle, ad.min_x, 0).await? as usize,
            min_y: snapshot_int32(handle, ad.min_y, 0).await? as usize,
            acquire_time: snapshot_float64(handle, ad.acquire_time, 0).await?,
            acquire_period: snapshot_float64(handle, ad.acquire_period, 0).await?,
            image_mode: ImageMode::from_i32(snapshot_int32(handle, ad.image_mode, 0).await?),
            num_images: snapshot_int32(handle, ad.num_images, 0).await?,
            array_callbacks: snapshot_int32(handle, ad.base.array_callbacks, 0).await? != 0,
            wait_for_plugins: snapshot_int32(handle, ad.base.wait_for_plugins, 0).await? != 0,
        })
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
            min_x: base.get_int32_param(ad.min_x, 0).unwrap_or(0) as usize,
            min_y: base.get_int32_param(ad.min_y, 0).unwrap_or(0) as usize,
            acquire_time: undefined_as_zero(base.get_float64_param(ad.acquire_time, 0))?,
            acquire_period: undefined_as_zero(base.get_float64_param(ad.acquire_period, 0))?,
            image_mode: ImageMode::from_i32(undefined_as_zero(
                base.get_int32_param(ad.image_mode, 0),
            )?),
            num_images: undefined_as_zero(base.get_int32_param(ad.num_images, 0))?,
            array_callbacks: base
                .get_int32_param(ad.base.array_callbacks, 0)
                .unwrap_or(0)
                != 0,
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
}
