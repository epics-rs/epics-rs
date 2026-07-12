use crate::error::AsynResult;
use crate::user::AsynUser;

/// Key/value configuration interface (asynOption equivalent).
///
/// `set_option` carries the caller's [`AsynUser`] because C's does
/// (`setOption(void *drvPvt, asynUser *pasynUser, key, val)`): its `timeout`
/// bounds any wire traffic the write causes, e.g. the RFC 2217 negotiation of
/// `asynInterposeCom.c:475,495`. `get_option` carries none: no option read in
/// this crate touches the wire — C's COM `getOption` (:657-725) answers from
/// cached state — and the `asynUser`'s other role there, `errorMessage`, is the
/// `Err` arm here.
pub trait AsynOption: Send + Sync {
    fn get_option(&self, key: &str) -> AsynResult<String>;
    fn set_option(&mut self, user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()>;
}
