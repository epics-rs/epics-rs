use crate::error::AsynResult;
use crate::user::AsynUser;

/// 64-bit integer I/O interface (asynInt64 equivalent).
pub trait AsynInt64: Send + Sync {
    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64>;
    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()>;
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(i64, i64)> {
        Ok((i64::MIN, i64::MAX))
    }
}
