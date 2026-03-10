use crate::error::AsynResult;

/// Key/value configuration interface (asynOption equivalent).
pub trait AsynOption: Send + Sync {
    fn get_option(&self, key: &str) -> AsynResult<String>;
    fn set_option(&mut self, key: &str, value: &str) -> AsynResult<()>;
}
