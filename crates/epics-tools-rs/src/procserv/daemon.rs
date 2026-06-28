//! Daemonize + signal forwarding.
//!
//! Mirrors C `forkAndGo()` (`procServ.cc:870`) and the
//! `OnSig{Pipe,Term,Hup}` handlers. Concretely:
//!
//! 1. **Detach from controlling terminal**: a single `fork` (as C does),
//!    then in the child redirect 0/1/2 to `/dev/null` and `setsid`. Like
//!    C `forkAndGo`, it does NOT `chdir("/")` — the daemon stays in the
//!    launch directory so relative paths resolve as C's do. The parent
//!    writes the pid file _with the daemon child's pid_ before exiting,
//!    so a `Type=forking` systemd unit can read it the instant the
//!    foreground command returns (procServ.cc:896-898).
//! 2. **Signal forwarding**:
//!    - `SIGTERM`/`SIGINT` — graceful shutdown
//!    - `SIGPIPE` — ignored (PTY writes to dead clients raise it)
//!    - `SIGHUP` — reopen the log file (logrotate support); handled by
//!      the supervisor, NOT here, because it owns the log handle and
//!      must keep running. C `OnSigHup` sets a flag the main loop turns
//!      into `openLogFile()` (`procServ.cc:641-645`), never a shutdown.

use std::os::fd::AsRawFd;
use std::path::Path;

use nix::fcntl::{OFlag, open};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, dup2, fork, setsid};
use tokio::sync::oneshot;

use crate::procserv::error::{ProcServError, ProcServResult};
use crate::procserv::sidecar::write_pid_file;

/// Side-effects the foreground command performs _on behalf of_ the
/// daemon child before it exits, mirroring C `forkAndGo`'s parent branch
/// (procServ.cc:887-899): announce the daemon's pid, warn about a missing
/// log file, and write the pid file with the **child's** pid so a
/// `Type=forking` systemd unit finds it the instant the parent returns.
pub struct DaemonParent<'a> {
    /// Diagnostic prefix (C `procservName` = argv[0]); the bin passes
    /// `"procserv-rs"` to match its other stderr diagnostics.
    pub name: &'a str,
    /// `--pidfile` path. The parent writes it with the daemon's pid.
    pub pid_path: Option<&'a Path>,
    /// `--quiet`: suppress the spawn notice and no-log-file warning
    /// (C gates both on `!quiet`, procServ.cc:889).
    pub quiet: bool,
    /// Whether a `--logfile` was configured (drives the warning text).
    pub has_logfile: bool,
    /// Whether a log *port* exists (refines the warning, procServ.cc:892).
    pub has_logport: bool,
}

/// Daemonize the current process. Equivalent to C `forkAndGo()`
/// (procServ.cc:870-912) when `--foreground` is not set.
///
/// A **single** `fork` (as C does — not the Stevens double fork): the
/// parent is the foreground command systemd waits on, the child becomes
/// the daemon. Concretely:
/// 1. `fork()`
/// 2. **Parent**: print the spawn notice + no-log-file warning (unless
///    `quiet`), write the pid file with the **child's** pid, `exit(0)`.
///    Writing the pid file here — before the parent returns — is what
///    closes the `Type=forking` systemd race (procServ.cc:896-898).
/// 3. **Child**: redirect stdin/stdout/stderr to `/dev/null`, then
///    `setsid()` so it is a session leader with no controlling tty.
///
/// We deliberately do NOT `chdir("/")` (nor `umask(0)`), matching C
/// `forkAndGo`: the daemon must stay in the launch directory so the
/// child and relative log/pid/info paths resolve there (procServ PS-15).
///
/// MUST be called BEFORE the tokio runtime starts; otherwise the
/// runtime's worker threads survive in the parent (they don't
/// transfer across fork). The bin entry handles this ordering.
pub fn fork_and_go(parent: DaemonParent<'_>) -> ProcServResult<()> {
    // Open /dev/null up front (C does this before the fork,
    // procServ.cc:875-880) so a failure is reported by the foreground
    // process rather than the detached daemon. With 0/1/2 still bound to
    // the launching tty, this fd is >= 3.
    let null = open("/dev/null", OFlag::O_RDWR, Mode::empty())
        .map_err(|e| ProcServError::Forkpty(format!("open /dev/null: {e}")))?;

    // SAFETY: the tokio runtime has not started, so the process is
    // single-threaded; fork is safe per POSIX.
    match unsafe { fork() }.map_err(|e| ProcServError::Forkpty(format!("fork: {e}")))? {
        ForkResult::Parent { child } => {
            // The foreground command. Announce + publish the pid file with
            // the daemon's pid BEFORE exiting (procServ.cc:887-899).
            let daemon_pid = child.as_raw();
            if !parent.quiet {
                eprintln!("{}: spawning daemon process: {daemon_pid}", parent.name);
                if !parent.has_logfile {
                    eprintln!(
                        "Warning: No log file{} specified.",
                        if parent.has_logport {
                            ""
                        } else {
                            " and no port for log connections"
                        }
                    );
                }
            }
            if let Some(p) = parent.pid_path
                && let Err(e) = write_pid_file(p, daemon_pid)
            {
                // C `writePidFile` logs and continues on failure
                // (procServ.cc:130-137); match that best-effort behavior.
                eprintln!(
                    "{}: cannot write pid file {}: {e}",
                    parent.name,
                    p.display()
                );
            }
            // `exit(0)` skips destructors, so the /dev/null OwnedFd is left
            // for the OS to reclaim; that is fine, the parent is leaving.
            std::process::exit(0);
        }
        ForkResult::Child => {
            // The background daemon. Deliberately NO `chdir("/")`: C
            // `forkAndGo` never chdir's (procServ.cc:870-912), so the daemon
            // stays in the launch directory. That is load-bearing — C
            // defaults the child's `chDir` to `myDir` (the startup cwd,
            // procServ.cc:220-221) and relative log/pid/info paths resolve
            // against it (procServ PS-15).
            //
            // Redirect stdio to /dev/null, then detach (C order,
            // procServ.cc:904-910). The `null` OwnedFd closes when it drops
            // at function return, after the dup2s have copied it onto 0/1/2.
            let null_fd = null.as_raw_fd();
            for fd in [0, 1, 2] {
                dup2(null_fd, fd)
                    .map_err(|e| ProcServError::Forkpty(format!("dup2(/dev/null, {fd}): {e}")))?;
            }
            setsid().map_err(|e| ProcServError::Forkpty(format!("setsid: {e}")))?;
            Ok(())
        }
    }
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
