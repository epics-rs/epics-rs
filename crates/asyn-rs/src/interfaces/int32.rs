use crate::error::AsynResult;
use crate::user::AsynUser;

/// 32-bit integer I/O interface (asynInt32 equivalent).
pub trait AsynInt32: Send + Sync {
    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32>;
    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()>;
    /// C `asynInt32Base.c:99` default: a driver that does not implement
    /// getBounds reports `low = high = 0`, so `convertAi`/`convertAo`
    /// skip the LINEAR ESLO/EOFF computation (`devAsynInt32.c:444`,
    /// `if (deviceHigh != deviceLow)`). Returning the full type span
    /// would instead run a meaningless conversion over a 2^32 range.
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
        Ok((0, 0))
    }
}
