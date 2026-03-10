use crate::error::AsynResult;
use crate::user::AsynUser;

/// 64-bit float I/O interface (asynFloat64 equivalent).
pub trait AsynFloat64: Send + Sync {
    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64>;
    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()>;
}
