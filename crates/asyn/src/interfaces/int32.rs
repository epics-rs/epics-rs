use crate::error::AsynResult;
use crate::user::AsynUser;

/// 32-bit integer I/O interface (asynInt32 equivalent).
pub trait AsynInt32: Send + Sync {
    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32>;
    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()>;
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
        Ok((i32::MIN, i32::MAX))
    }
}
