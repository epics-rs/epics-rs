use crate::error::AsynResult;
use crate::user::AsynUser;

/// Connection management interface (asynCommon equivalent).
pub trait AsynCommon: Send + Sync {
    fn connect(&mut self, user: &AsynUser) -> AsynResult<()>;
    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()>;
    fn report(&self, level: i32);
}
