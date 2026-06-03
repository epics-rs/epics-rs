use crate::error::AsynResult;
use crate::user::AsynUser;

/// 64-bit integer I/O interface (asynInt64 equivalent).
pub trait AsynInt64: Send + Sync {
    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64>;
    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()>;
    /// C `asynInt64Base.c:99` default: a driver that does not implement
    /// getBounds reports `low = high = 0`, so `convertAi`/`convertAo`
    /// skip the LINEAR ESLO/EOFF computation (`devAsynInt64.c`,
    /// `if (deviceHigh != deviceLow)`). Returning the full type span
    /// would instead run a meaningless conversion over a 2^64 range.
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(i64, i64)> {
        Ok((0, 0))
    }
}
