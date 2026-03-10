use crate::error::AsynResult;
use crate::user::AsynUser;

/// Digital I/O with bit mask (asynUInt32Digital equivalent).
pub trait AsynUInt32Digital: Send + Sync {
    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32>;
    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()>;
}
