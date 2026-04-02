use std::sync::Arc;

use crate::error::AsynResult;
use crate::param::EnumEntry;
use crate::user::AsynUser;

/// Interface for enumeration I/O.
pub trait AsynEnum: Send + Sync {
    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)>;
    fn write_enum(&mut self, user: &mut AsynUser, index: usize) -> AsynResult<()>;
    fn write_enum_choices(
        &mut self,
        user: &mut AsynUser,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()>;
}
