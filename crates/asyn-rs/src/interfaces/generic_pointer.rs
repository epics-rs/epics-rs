use std::any::Any;
use std::sync::Arc;

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Interface for opaque pointer I/O.
pub trait AsynGenericPointer: Send + Sync {
    fn read_generic_pointer(&mut self, user: &AsynUser) -> AsynResult<Arc<dyn Any + Send + Sync>>;
    fn write_generic_pointer(
        &mut self,
        user: &mut AsynUser,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()>;
}
