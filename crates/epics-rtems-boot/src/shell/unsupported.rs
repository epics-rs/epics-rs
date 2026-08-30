//! The backend for every build with no RTEMS shell behind it — a host build,
//! and the toolchain-free portability build where the C was never compiled.
//!
//! Every entry point refuses, and that is the whole point of the file. The
//! alternative shape — returning `Ok` having printed nothing — is what a stub
//! would do, and it is worse than the command's absence: an operator reading a
//! boot log cannot tell a `netstat` that printed nothing from an interface
//! table that is genuinely empty, and `help` would list a command that does
//! nothing on a machine where it can never do anything.
//!
//! Nothing here is reachable from a hosted IOC in practice: the commands are
//! registered only from the RTEMS IOC binaries. The refusals exist so that a
//! host `cargo test` can hold the funnel to its contract without a board.

use super::ShellError;

pub(super) fn netstat(_level: i32) -> Result<(), ShellError> {
    Err(ShellError::Unsupported)
}

pub(super) fn run_shell_command(_name: &str, _argv: &[String]) -> Result<i32, ShellError> {
    Err(ShellError::Unsupported)
}

pub(super) fn set_log_priority(_name: &str) -> Result<(), ShellError> {
    Err(ShellError::Unsupported)
}

/// Empty rather than a guess at the C library's list: the names a target's
/// `setlogmask` accepts are that target's `prioritynames`, and this build has
/// none to read.
pub(super) fn log_priority_names() -> Vec<String> {
    Vec::new()
}
