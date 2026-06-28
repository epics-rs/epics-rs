//! Daemonize + signal forwarding.
//!
//! Mirrors C `forkAndGo()` (`procServ.cc:870`) and the
//! `OnSig{Pipe,Term,Hup}` handlers. Concretely:
//!
//! 1. **Detach from controlling terminal**: double `fork` + `setsid`,
//!    redirect 0/1/2 to `/dev/null`. Like C `forkAndGo`, it does NOT
//!    `chdir("/")` — the daemon stays in the launch directory so
//!    relative paths resolve as C's do.
//! 2. **Signal forwarding**:
//!    - `SIGTERM`/`SIGINT` — graceful shutdown
//!    - `SIGPIPE` — ignored (PTY writes to dead clients raise it)
//!    - `SIGHUP` — reopen the log file (logrotate support); handled by
//!      the supervisor, NOT here, because it owns the log handle and
//!      must keep running. C `OnSigHup` sets a flag the main loop turns
//!      into `openLogFile()` (`procServ.cc:641-645`), never a shutdown.

use std::os::fd::AsRawFd;

use nix::fcntl::{OFlag, open};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, close, dup2, fork, setsid};
use tokio::sync::oneshot;

use crate::procserv::error::{ProcServError, ProcServResult};

/// Daemonize the current process. Equivalent to C `forkAndGo()`
/// when `--foreground` is not set.
///
/// Steps (Stevens-style daemonization, minus the `chdir("/")` C omits):
/// 1. `fork()`; parent exits, child continues
/// 2. `setsid()` — child becomes session leader, no controlling tty
/// 3. Second `fork()` — grandchild can never re-acquire a tty
/// 4. Redirect stdin/stdout/stderr to `/dev/null`
///
/// We deliberately do NOT `chdir("/")` (nor `umask(0)`), matching C
/// `forkAndGo`: the daemon must stay in the launch directory so the
/// child and relative log/pid/info paths resolve there (procServ PS-15).
///
/// MUST be called BEFORE the tokio runtime starts; otherwise the
/// runtime's worker threads survive in the parent (they don't
/// transfer across fork). The bin entry handles this ordering.
pub fn fork_and_go() -> ProcServResult<()> {
    // First fork.
    // SAFETY: we have not yet started the tokio runtime, so the
    // process is single-threaded; fork is safe per POSIX.
    match unsafe { fork() }.map_err(|e| ProcServError::Forkpty(format!("first fork: {e}")))? {
        ForkResult::Parent { .. } => {
            // Parent exits cleanly.
            std::process::exit(0);
        }
        ForkResult::Child => {}
    }

    setsid().map_err(|e| ProcServError::Forkpty(format!("setsid: {e}")))?;

    // Second fork — daemon can never re-acquire a controlling tty.
    match unsafe { fork() }.map_err(|e| ProcServError::Forkpty(format!("second fork: {e}")))? {
        ForkResult::Parent { .. } => {
            std::process::exit(0);
        }
        ForkResult::Child => {}
    }

    // Deliberately NO `chdir("/")`. C `forkAndGo` never chdir's
    // (procServ.cc:870-912), so the daemon stays in the launch directory.
    // That is load-bearing: C defaults the child's `chDir` to `myDir` (the
    // startup cwd, procServ.cc:220-221) and relative log/pid/info paths
    // resolve against it. chdir-ing to `/` here would break a child started
    // with a relative `st.cmd` and mis-report the startup directory in the
    // welcome banner (procServ PS-15).

    // Redirect stdin/stdout/stderr to /dev/null.
    let null = open("/dev/null", OFlag::O_RDWR, Mode::empty())
        .map_err(|e| ProcServError::Forkpty(format!("open /dev/null: {e}")))?;
    let null_fd = null.as_raw_fd();
    for fd in [0, 1, 2] {
        dup2(null_fd, fd)
            .map_err(|e| ProcServError::Forkpty(format!("dup2(/dev/null, {fd}): {e}")))?;
    }
    if null_fd > 2 {
        let _ = close(null_fd);
    }

    Ok(())
}

/// Set up the signal-handling layer. Returns a future that resolves
/// when a graceful-shutdown signal arrives. Must be called from
/// inside the tokio runtime — uses `tokio::signal::unix`.
///
/// `SIGPIPE` is set to ignored synchronously (via `nix::sys::signal`)
/// so a write to a dead client socket doesn't kill the supervisor.
/// `SIGTERM`/`SIGINT` are converted to a single [`ShutdownSignal`]
/// future. `SIGHUP` is deliberately NOT handled here — it means
/// "reopen the log file" (logrotate), which the supervisor owns; if it
/// were folded into the shutdown set a logrotate `kill -HUP` would tear
/// the IOC down.
pub async fn install_signal_handlers() -> ProcServResult<ShutdownSignal> {
    // SIGPIPE → ignore. tokio::signal doesn't expose SIG_IGN
    // directly, so use nix.
    // SAFETY: signal(SIGPIPE, SIG_IGN) is async-signal-safe and
    // disposition-only — no userspace handler installed.
    unsafe {
        signal(Signal::SIGPIPE, SigHandler::SigIgn)
            .map_err(|e| ProcServError::Forkpty(format!("ignore SIGPIPE: {e}")))?;
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(ProcServError::Io)?;
    let mut intr = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(ProcServError::Io)?;

    let (tx, rx) = oneshot::channel::<ShutdownReason>();

    tokio::spawn(async move {
        let reason = tokio::select! {
            _ = term.recv() => ShutdownReason::Terminate,
            _ = intr.recv() => ShutdownReason::Interrupt,
        };
        let _ = tx.send(reason);
    });

    Ok(ShutdownSignal { rx })
}

/// Why the shutdown signal fired. SIGHUP is intentionally absent — it
/// is a log-reopen request, handled by the supervisor, not a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Terminate,
    Interrupt,
}

/// Future-like handle that resolves when a graceful-shutdown signal
/// is received. The supervisor task `tokio::select!`s on this
/// alongside its other branches.
pub struct ShutdownSignal {
    rx: oneshot::Receiver<ShutdownReason>,
}

impl ShutdownSignal {
    /// Wait for the shutdown trigger. Returns the reason it fired
    /// or [`ProcServError::Shutdown`] if the sending end was
    /// dropped (which can't happen unless the signal task panicked).
    pub async fn wait(self) -> ProcServResult<ShutdownReason> {
        self.rx.await.map_err(|_| ProcServError::Shutdown)
    }
}

#[cfg(test)]
mod tests {
    // We don't unit-test fork_and_go (forking from cargo test is
    // hostile) or signal handlers (process-wide state). Both are
    // exercised by the integration test that spawns procserv-rs as
    // a child.
}
