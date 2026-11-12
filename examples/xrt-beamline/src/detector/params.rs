use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::undefined_as_zero;

use ad_core_rs::driver::ImageMode;
use ad_core_rs::params::ADBaseParams;

use crate::beamline_sim::MotorPositions;
use crate::detector::XrtDetector;

/// XRT detector parameter indices (beyond ADBase).
#[derive(Clone, Copy)]
pub struct XrtDetectorParams {
    // Undulator motors
    pub und_gap: usize,
    pub und_x: usize,
    pub und_z: usize,

    // DCM motors
    pub dcm_theta: usize,
    pub dcm_theta2: usize,
    pub dcm_y: usize,
    pub dcm_chi1: usize,
    pub dcm_chi2: usize,
    pub dcm_z: usize,

    // HFM motors
    pub hfm_pitch: usize,
    pub hfm_roll: usize,
    pub hfm_yaw: usize,
    pub hfm_x: usize,
    pub hfm_y: usize,
    pub hfm_z: usize,
    pub hfm_r_major: usize,
    pub hfm_r_minor: usize,

    // VFM motors
    pub vfm_pitch: usize,
    pub vfm_roll: usize,
    pub vfm_yaw: usize,
    pub vfm_x: usize,
    pub vfm_y: usize,
    pub vfm_z: usize,
    pub vfm_r_major: usize,
    pub vfm_r_minor: usize,

    // Simulation readbacks
    pub sim_source_energy: usize,
    pub sim_dcm_energy: usize,
    pub sim_efficiency: usize,
    pub sim_flux: usize,
    pub sim_centroid_x: usize,
    pub sim_centroid_z: usize,
    pub sim_fwhm_x: usize,
    pub sim_fwhm_z: usize,
    pub sim_rms_x: usize,
    pub sim_rms_z: usize,
    pub sim_nrays: usize,
}

impl XrtDetectorParams {
    pub fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            und_gap: base.create_param("XRT_UND_GAP", ParamType::Float64)?,
            und_x: base.create_param("XRT_UND_X", ParamType::Float64)?,
            und_z: base.create_param("XRT_UND_Z", ParamType::Float64)?,

            dcm_theta: base.create_param("XRT_DCM_THETA", ParamType::Float64)?,
            dcm_theta2: base.create_param("XRT_DCM_THETA2", ParamType::Float64)?,
            dcm_y: base.create_param("XRT_DCM_Y", ParamType::Float64)?,
            dcm_chi1: base.create_param("XRT_DCM_CHI1", ParamType::Float64)?,
            dcm_chi2: base.create_param("XRT_DCM_CHI2", ParamType::Float64)?,
            dcm_z: base.create_param("XRT_DCM_Z", ParamType::Float64)?,

            hfm_pitch: base.create_param("XRT_HFM_PITCH", ParamType::Float64)?,
            hfm_roll: base.create_param("XRT_HFM_ROLL", ParamType::Float64)?,
            hfm_yaw: base.create_param("XRT_HFM_YAW", ParamType::Float64)?,
            hfm_x: base.create_param("XRT_HFM_X", ParamType::Float64)?,
            hfm_y: base.create_param("XRT_HFM_Y", ParamType::Float64)?,
            hfm_z: base.create_param("XRT_HFM_Z", ParamType::Float64)?,
            hfm_r_major: base.create_param("XRT_HFM_R_MAJOR", ParamType::Float64)?,
            hfm_r_minor: base.create_param("XRT_HFM_R_MINOR", ParamType::Float64)?,

            vfm_pitch: base.create_param("XRT_VFM_PITCH", ParamType::Float64)?,
            vfm_roll: base.create_param("XRT_VFM_ROLL", ParamType::Float64)?,
            vfm_yaw: base.create_param("XRT_VFM_YAW", ParamType::Float64)?,
            vfm_x: base.create_param("XRT_VFM_X", ParamType::Float64)?,
            vfm_y: base.create_param("XRT_VFM_Y", ParamType::Float64)?,
            vfm_z: base.create_param("XRT_VFM_Z", ParamType::Float64)?,
            vfm_r_major: base.create_param("XRT_VFM_R_MAJOR", ParamType::Float64)?,
            vfm_r_minor: base.create_param("XRT_VFM_R_MINOR", ParamType::Float64)?,

            sim_source_energy: base.create_param("XRT_SIM_SRC_E", ParamType::Float64)?,
            sim_dcm_energy: base.create_param("XRT_SIM_DCM_E", ParamType::Float64)?,
            sim_efficiency: base.create_param("XRT_SIM_EFF", ParamType::Float64)?,
            sim_flux: base.create_param("XRT_SIM_FLUX", ParamType::Float64)?,
            sim_centroid_x: base.create_param("XRT_SIM_CX", ParamType::Float64)?,
            sim_centroid_z: base.create_param("XRT_SIM_CZ", ParamType::Float64)?,
            sim_fwhm_x: base.create_param("XRT_SIM_FWHM_X", ParamType::Float64)?,
            sim_fwhm_z: base.create_param("XRT_SIM_FWHM_Z", ParamType::Float64)?,
            sim_rms_x: base.create_param("XRT_SIM_RMS_X", ParamType::Float64)?,
            sim_rms_z: base.create_param("XRT_SIM_RMS_Z", ParamType::Float64)?,
            sim_nrays: base.create_param("XRT_SIM_NRAYS", ParamType::Int32)?,
        })
    }

    /// List of all motor parameter indices (for dirty flag checking).
    pub fn motor_params(&self) -> Vec<usize> {
        vec![
            self.und_gap,
            self.und_x,
            self.und_z,
            self.dcm_theta,
            self.dcm_theta2,
            self.dcm_y,
            self.dcm_chi1,
            self.dcm_chi2,
            self.dcm_z,
            self.hfm_pitch,
            self.hfm_roll,
            self.hfm_yaw,
            self.hfm_x,
            self.hfm_y,
            self.hfm_z,
            self.hfm_r_major,
            self.hfm_r_minor,
            self.vfm_pitch,
            self.vfm_roll,
            self.vfm_yaw,
            self.vfm_x,
            self.vfm_y,
            self.vfm_z,
            self.vfm_r_major,
            self.vfm_r_minor,
        ]
    }
}

/// Snapshot of all config needed for one simulation step.
pub struct XrtConfigSnapshot {
    pub motors: MotorPositions,
    pub acquire_time: f64,
    pub acquire_period: f64,
    pub image_mode: ImageMode,
    pub num_images: i32,
    pub array_callbacks: bool,
    pub wait_for_plugins: bool,
}

impl XrtConfigSnapshot {
    /// [`Self::read_from`] under one hold of the port, as a C driver task
    /// reads its config with `getIntegerParam` between one `lock()` and
    /// `unlock()`; a round trip per parameter kept Acquire busy for each one.
    pub async fn read_via_handle(handle: &PortHandle) -> AsynResult<Self> {
        handle
            .with_driver(|d: &mut XrtDetector| {
                Self::read_from(&d.ad.port_base, &d.ad.params, &d.xrt_params)
            })
            .await?
    }

    pub fn read_from(
        base: &PortDriverBase,
        ad: &ADBaseParams,
        xrt: &XrtDetectorParams,
    ) -> AsynResult<Self> {
        Ok(Self {
            motors: MotorPositions {
                und_gap: undefined_as_zero(base.get_float64_param(xrt.und_gap, 0))?,
                und_x: undefined_as_zero(base.get_float64_param(xrt.und_x, 0))?,
                und_z: undefined_as_zero(base.get_float64_param(xrt.und_z, 0))?,
                dcm_theta: undefined_as_zero(base.get_float64_param(xrt.dcm_theta, 0))?,
                dcm_theta2: undefined_as_zero(base.get_float64_param(xrt.dcm_theta2, 0))?,
                dcm_y: undefined_as_zero(base.get_float64_param(xrt.dcm_y, 0))?,
                dcm_chi1: undefined_as_zero(base.get_float64_param(xrt.dcm_chi1, 0))?,
                dcm_chi2: undefined_as_zero(base.get_float64_param(xrt.dcm_chi2, 0))?,
                dcm_z: undefined_as_zero(base.get_float64_param(xrt.dcm_z, 0))?,
                hfm_pitch: undefined_as_zero(base.get_float64_param(xrt.hfm_pitch, 0))?,
                hfm_roll: undefined_as_zero(base.get_float64_param(xrt.hfm_roll, 0))?,
                hfm_yaw: undefined_as_zero(base.get_float64_param(xrt.hfm_yaw, 0))?,
                hfm_x: undefined_as_zero(base.get_float64_param(xrt.hfm_x, 0))?,
                hfm_y: undefined_as_zero(base.get_float64_param(xrt.hfm_y, 0))?,
                hfm_z: undefined_as_zero(base.get_float64_param(xrt.hfm_z, 0))?,
                hfm_r_major: undefined_as_zero(base.get_float64_param(xrt.hfm_r_major, 0))?,
                hfm_r_minor: undefined_as_zero(base.get_float64_param(xrt.hfm_r_minor, 0))?,
                vfm_pitch: undefined_as_zero(base.get_float64_param(xrt.vfm_pitch, 0))?,
                vfm_roll: undefined_as_zero(base.get_float64_param(xrt.vfm_roll, 0))?,
                vfm_yaw: undefined_as_zero(base.get_float64_param(xrt.vfm_yaw, 0))?,
                vfm_x: undefined_as_zero(base.get_float64_param(xrt.vfm_x, 0))?,
                vfm_y: undefined_as_zero(base.get_float64_param(xrt.vfm_y, 0))?,
                vfm_z: undefined_as_zero(base.get_float64_param(xrt.vfm_z, 0))?,
                vfm_r_major: undefined_as_zero(base.get_float64_param(xrt.vfm_r_major, 0))?,
                vfm_r_minor: undefined_as_zero(base.get_float64_param(xrt.vfm_r_minor, 0))?,
            },
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
