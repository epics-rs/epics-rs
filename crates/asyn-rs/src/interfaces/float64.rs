use crate::error::AsynResult;
use crate::user::AsynUser;

/// 64-bit float I/O interface (asynFloat64 equivalent).
pub trait AsynFloat64: Send + Sync {
    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64>;
    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()>;
    /// Driver-reported low/high bounds for this parameter. Mirrors
    /// asyn upstream issue #218 — drivers that have a fixed range
    /// (e.g. an ADC scale) advertise it through this hook so EPICS
    /// records can populate DRVL/DRVH at init.
    ///
    /// Default returns the full f64 range, which is the conservative
    /// "no bounds known" answer — drivers override it.
    fn get_limits(&self, _user: &AsynUser) -> AsynResult<(f64, f64)> {
        Ok((f64::NEG_INFINITY, f64::INFINITY))
    }
}
