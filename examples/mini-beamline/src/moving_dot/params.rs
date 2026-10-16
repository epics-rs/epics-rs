use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{snapshot_float64, snapshot_int32};

use ad_core_rs::driver::ImageMode;
use ad_core_rs::params::ADBaseParams;

/// MovingDot-specific parameter indices (beyond ADBase).
#[derive(Clone, Copy)]
pub struct MovingDotParams {
    pub motor_x_pos: usize,
    pub motor_y_pos: usize,
    pub beam_current: usize,
    pub shutter_open: usize,
    pub slit_left: usize,
    pub slit_right: usize,
    pub slit_top: usize,
    pub slit_bottom: usize,
}

impl MovingDotParams {
    pub fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            motor_x_pos: base.create_param("DOT_MOTOR_X_POS", ParamType::Float64)?,
            motor_y_pos: base.create_param("DOT_MOTOR_Y_POS", ParamType::Float64)?,
            beam_current: base.create_param("DOT_BEAM_CURRENT", ParamType::Float64)?,
            shutter_open: base.create_param("DOT_SHUTTER_OPEN", ParamType::Int32)?,
            slit_left: base.create_param("DOT_SLIT_LEFT", ParamType::Float64)?,
            slit_right: base.create_param("DOT_SLIT_RIGHT", ParamType::Float64)?,
            slit_top: base.create_param("DOT_SLIT_TOP", ParamType::Float64)?,
            slit_bottom: base.create_param("DOT_SLIT_BOTTOM", ParamType::Float64)?,
        })
    }
}

/// Snapshot of all configuration needed for one frame computation.
pub struct MovingDotConfigSnapshot {
    pub motor_x: f64,
    pub motor_y: f64,
    pub beam_current: f64,
    pub shutter_open: bool,
    pub acquire_time: f64,
    pub acquire_period: f64,
    pub image_mode: ImageMode,
    pub num_images: i32,
    pub array_callbacks: bool,
    pub wait_for_plugins: bool,
    pub size_x: usize,
    pub size_y: usize,
    pub bin_x: usize,
    pub bin_y: usize,
    pub slit_left: f64,
    pub slit_right: f64,
    pub slit_top: f64,
    pub slit_bottom: f64,
}

impl MovingDotConfigSnapshot {
    /// Read config via PortHandle (async). For use from the acquisition task thread.
    pub async fn read_via_handle(
        handle: &PortHandle,
        ad: &ADBaseParams,
        dot: &MovingDotParams,
    ) -> AsynResult<Self> {
        Ok(Self {
            motor_x: snapshot_float64(handle, dot.motor_x_pos, 0).await?,
            motor_y: snapshot_float64(handle, dot.motor_y_pos, 0).await?,
            beam_current: snapshot_float64(handle, dot.beam_current, 0).await?,
            shutter_open: snapshot_int32(handle, dot.shutter_open, 0).await? != 0,
            acquire_time: snapshot_float64(handle, ad.acquire_time, 0).await?,
            acquire_period: snapshot_float64(handle, ad.acquire_period, 0).await?,
            image_mode: ImageMode::from_i32(snapshot_int32(handle, ad.image_mode, 0).await?),
            num_images: snapshot_int32(handle, ad.num_images, 0).await?,
            array_callbacks: snapshot_int32(handle, ad.base.array_callbacks, 0).await? != 0,
            wait_for_plugins: snapshot_int32(handle, ad.base.wait_for_plugins, 0).await? != 0,
            size_x: snapshot_int32(handle, ad.size_x, 0).await? as usize,
            size_y: snapshot_int32(handle, ad.size_y, 0).await? as usize,
            bin_x: (snapshot_int32(handle, ad.bin_x, 0).await?).max(1) as usize,
            bin_y: (snapshot_int32(handle, ad.bin_y, 0).await?).max(1) as usize,
            slit_left: snapshot_float64(handle, dot.slit_left, 0).await?,
            slit_right: snapshot_float64(handle, dot.slit_right, 0).await?,
            slit_top: snapshot_float64(handle, dot.slit_top, 0).await?,
            slit_bottom: snapshot_float64(handle, dot.slit_bottom, 0).await?,
        })
    }
}
