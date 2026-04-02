pub mod params;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::user::AsynUser;

use crate::physics::{self, DetectorMode};
use params::PointDetectorParams;

/// A simple point detector that recomputes its output on every parameter write.
///
/// One Rust type, instantiated 3 times with different DetectorMode.
pub struct PointDetector {
    base: PortDriverBase,
    pub params: PointDetectorParams,
    mode: DetectorMode,
}

impl PointDetector {
    pub fn new(port_name: &str, mode: DetectorMode) -> AsynResult<Self> {
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        let params = PointDetectorParams::create(&mut base)?;

        // Set defaults
        base.set_float64_param(params.motor_pos, 0, 0.0)?;
        base.set_float64_param(params.beam_current, 0, 500.0)?;
        base.set_float64_param(params.exposure_time, 0, 1.0)?;
        base.set_float64_param(params.det_value, 0, 0.0)?;
        base.set_int32_param(params.det_mode, 0, mode as i32)?;
        base.set_float64_param(params.det_sigma, 0, physics::default_sigma(mode))?;
        base.set_float64_param(params.det_center, 0, physics::default_center(mode))?;

        let mut det = Self { base, params, mode };
        det.recompute()?;
        Ok(det)
    }

    /// Recompute the detector output value from current params.
    fn recompute(&mut self) -> AsynResult<()> {
        let mtr = self.base.get_float64_param(self.params.motor_pos, 0)?;
        let current = self.base.get_float64_param(self.params.beam_current, 0)?;
        let exposure = self.base.get_float64_param(self.params.exposure_time, 0)?;
        let sigma = self.base.get_float64_param(self.params.det_sigma, 0)?;
        let center = self.base.get_float64_param(self.params.det_center, 0)?;

        let value = physics::point_reading(self.mode, mtr, current, exposure, sigma, center);

        self.base
            .set_float64_param(self.params.det_value, 0, value)?;
        Ok(())
    }
}

impl PortDriver for PointDetector {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        self.base.params.set_float64(reason, user.addr, value)?;

        // Recompute if an input parameter changed
        if reason == self.params.motor_pos
            || reason == self.params.beam_current
            || reason == self.params.exposure_time
            || reason == self.params.det_sigma
            || reason == self.params.det_center
        {
            self.recompute()?;
        }

        self.base.call_param_callbacks(0)?;
        Ok(())
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base.params.set_int32(user.reason, user.addr, value)?;
        self.base.call_param_callback(0, user.reason)?;
        Ok(())
    }
}

/// Handle to a running PointDetector runtime.
pub struct PointDetectorRuntime {
    pub runtime_handle: PortRuntimeHandle,
    pub params: PointDetectorParams,
}

impl PointDetectorRuntime {
    pub fn port_handle(&self) -> &asyn_rs::port_handle::PortHandle {
        self.runtime_handle.port_handle()
    }
}

/// Create a PointDetector with actor-based runtime.
pub fn create_point_detector(
    port_name: &str,
    mode: DetectorMode,
) -> AsynResult<PointDetectorRuntime> {
    let det = PointDetector::new(port_name, mode)?;
    let params = det.params;

    let (runtime_handle, _actor_jh) = create_port_runtime(det, RuntimeConfig::default());

    Ok(PointDetectorRuntime {
        runtime_handle,
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_pinhole() {
        let rt = create_point_detector("PH_TEST", DetectorMode::PinHole).unwrap();
        let handle = rt.port_handle();
        let val = handle
            .read_float64_blocking(rt.params.det_value, 0)
            .unwrap();
        // With default motor_pos=0, center=0 for pinhole, should get max reading
        assert!(val > 0.0);
    }

    #[test]
    fn test_recompute_on_motor_write() {
        let rt = create_point_detector("PH_RECOMP", DetectorMode::PinHole).unwrap();
        let handle = rt.port_handle();

        // At center: large value
        handle
            .write_float64_blocking(rt.params.motor_pos, 0, 0.0)
            .unwrap();
        let val_center = handle
            .read_float64_blocking(rt.params.det_value, 0)
            .unwrap();

        // Far away: small value
        handle
            .write_float64_blocking(rt.params.motor_pos, 0, 50.0)
            .unwrap();
        let val_far = handle
            .read_float64_blocking(rt.params.det_value, 0)
            .unwrap();

        assert!(
            val_center > val_far * 10.0,
            "center={val_center}, far={val_far}"
        );
    }

    #[test]
    fn test_create_edge() {
        let rt = create_point_detector("EDGE_TEST", DetectorMode::Edge).unwrap();
        let handle = rt.port_handle();
        let val = handle
            .read_float64_blocking(rt.params.det_value, 0)
            .unwrap();
        assert!(val > 0.0);
    }

    #[test]
    fn test_create_slit() {
        let rt = create_point_detector("SLIT_TEST", DetectorMode::Slit).unwrap();
        let handle = rt.port_handle();
        let val = handle
            .read_float64_blocking(rt.params.det_value, 0)
            .unwrap();
        assert!(val > 0.0);
    }
}
