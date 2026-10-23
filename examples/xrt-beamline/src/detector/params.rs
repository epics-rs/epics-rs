use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;

use ad_core_rs::driver::{snapshot_float64, snapshot_int32};

use ad_core_rs::driver::ImageMode;
use ad_core_rs::params::ADBaseParams;

use crate::beamline_sim::MotorPositions;

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
    pub async fn read_via_handle(
        handle: &PortHandle,
        ad: &ADBaseParams,
        xrt: &XrtDetectorParams,
    ) -> AsynResult<Self> {
        Ok(Self {
            motors: MotorPositions {
                und_gap: snapshot_float64(handle, xrt.und_gap, 0).await?,
                und_x: snapshot_float64(handle, xrt.und_x, 0).await?,
                und_z: snapshot_float64(handle, xrt.und_z, 0).await?,
                dcm_theta: snapshot_float64(handle, xrt.dcm_theta, 0).await?,
                dcm_theta2: snapshot_float64(handle, xrt.dcm_theta2, 0).await?,
                dcm_y: snapshot_float64(handle, xrt.dcm_y, 0).await?,
                dcm_chi1: snapshot_float64(handle, xrt.dcm_chi1, 0).await?,
                dcm_chi2: snapshot_float64(handle, xrt.dcm_chi2, 0).await?,
                dcm_z: snapshot_float64(handle, xrt.dcm_z, 0).await?,
                hfm_pitch: snapshot_float64(handle, xrt.hfm_pitch, 0).await?,
                hfm_roll: snapshot_float64(handle, xrt.hfm_roll, 0).await?,
                hfm_yaw: snapshot_float64(handle, xrt.hfm_yaw, 0).await?,
                hfm_x: snapshot_float64(handle, xrt.hfm_x, 0).await?,
                hfm_y: snapshot_float64(handle, xrt.hfm_y, 0).await?,
                hfm_z: snapshot_float64(handle, xrt.hfm_z, 0).await?,
                hfm_r_major: snapshot_float64(handle, xrt.hfm_r_major, 0).await?,
                hfm_r_minor: snapshot_float64(handle, xrt.hfm_r_minor, 0).await?,
                vfm_pitch: snapshot_float64(handle, xrt.vfm_pitch, 0).await?,
                vfm_roll: snapshot_float64(handle, xrt.vfm_roll, 0).await?,
                vfm_yaw: snapshot_float64(handle, xrt.vfm_yaw, 0).await?,
                vfm_x: snapshot_float64(handle, xrt.vfm_x, 0).await?,
                vfm_y: snapshot_float64(handle, xrt.vfm_y, 0).await?,
                vfm_z: snapshot_float64(handle, xrt.vfm_z, 0).await?,
                vfm_r_major: snapshot_float64(handle, xrt.vfm_r_major, 0).await?,
                vfm_r_minor: snapshot_float64(handle, xrt.vfm_r_minor, 0).await?,
            },
            acquire_time: snapshot_float64(handle, ad.acquire_time, 0).await?,
            acquire_period: snapshot_float64(handle, ad.acquire_period, 0).await?,
            image_mode: ImageMode::from_i32(snapshot_int32(handle, ad.image_mode, 0).await?),
            num_images: snapshot_int32(handle, ad.num_images, 0).await?,
            array_callbacks: snapshot_int32(handle, ad.base.array_callbacks, 0).await? != 0,
            wait_for_plugins: snapshot_int32(handle, ad.base.wait_for_plugins, 0).await? != 0,
        })
    }
}
