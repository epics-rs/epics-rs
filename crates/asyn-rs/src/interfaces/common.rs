use crate::error::AsynResult;
use crate::user::AsynUser;

/// Connection management interface (asynCommon equivalent).
pub trait AsynCommon: Send + Sync {
    fn connect(&mut self, user: &AsynUser) -> AsynResult<()>;
    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()>;
    /// C `asynCommon::report(void *drvPvt, FILE *fp, int details)` — the report
    /// goes to the stream the *caller* names, never to one the implementation
    /// picks. `asynReport` names stdout (asynShellCommands.c:589).
    fn report(&self, out: &mut dyn std::fmt::Write, level: i32);
}
