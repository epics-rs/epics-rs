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
    /// Diagnostic prefix (C `procservName` = `argv[0]`); the bin passes
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
                // C `ignore_result( dup(fh) )` (procServ.cc:906) — unchecked.
                // The parent has already written the pidfile with this child's
                // pid and `exit(0)`'d, so aborting here is a headless-daemon
                // false-success (PS-52, sibling of PS-50). The IOC's stdio
                // comes from the PTY, not the daemon's 0/1/2, so a failed
                // redirect doesn't break it — warn and continue.
                if let Err(e) = dup2(null_fd, fd) {
                    eprintln!("procserv-rs: dup2(/dev/null, {fd}) failed: {e}; continuing");
                }
            }
            // C `setsid();` (procServ.cc:910) — also unchecked. Failing to
            // detach from the controlling terminal does not break the daemon;
            // warn and continue rather than abort the already-published child.
            if let Err(e) = setsid() {
                eprintln!("procserv-rs: setsid failed: {e}; continuing");
            }
            Ok(())
        }
    }
}

/// Set up the signal-handling layer. Returns a future that resolves
/// when a graceful-shutdown signal arrives. Must be called from
/// inside the tokio runtime — uses `tokio::signal::unix`.
///
/// This is the single owner of the supervisor's signal *dispositions* —
/// and, because an ignored disposition survives `execve`, of every
/// disposition the child inherits that [`super::child`] does not
/// explicitly reset. Any new `SIG_IGN` that C sets in its parent belongs
/// here, not at a call site.
///
/// `SIGPIPE` is set to ignored synchronously (via `nix::sys::signal`)
/// so a write to a dead client socket doesn't kill the supervisor.
/// `SIGXFSZ` is ignored unconditionally, matching C
/// (`procServ.cc:502-503`): a write past `RLIMIT_FSIZE` — an oversized
/// log under a `ulimit -f` — must return `EFBIG` to the supervisor
/// rather than kill it, and the child inherits the ignored disposition
/// through `execvp` exactly as it does under C.
/// `SIGTERM` is converted to a [`ShutdownSignal`] future. `SIGHUP` is
/// deliberately NOT handled here — it means "reopen the log file"
/// (logrotate), which the supervisor owns; if it were folded into the
/// shutdown set a logrotate `kill -HUP` would tear the IOC down.
///
/// `in_fg_mode` selects between C's two dispositions for `SIGINT` /
/// `SIGQUIT` (`procServ.cc:503-509`):
///
/// * **foreground** — both are `SIG_IGN`. The launching terminal is the
///   operator's console session: `Ctrl-C` and `Ctrl-\` are keystrokes
///   the console client forwards to the child, not instructions to the
///   supervisor. Without this, `Ctrl-C` at a `procserv-rs -f` prompt
///   shuts the supervisor down and drops the IOC, and `Ctrl-\` kills it
///   with a core dump via the default `SIGQUIT` disposition.
/// * **daemon** — `SIGINT` joins `SIGTERM` as a shutdown trigger and
///   `SIGQUIT` keeps its default disposition, exactly as C leaves them
///   when `inFgMode` is false.
///
/// Registration is non-fatal. C installs every handler in the foreground
/// parent before `forkAndGo` (`procServ.cc:477,551`) with **unchecked**
/// `sigaction` (`procServ.cc:496-509`). Rust must register post-fork — the
/// tokio reactor can't survive `fork()` — so a failure here lands in the
/// daemon child, after the parent already reported success and wrote the
/// pidfile; aborting would be a headless-daemon false-success. Match C's
/// "never checks": a failed registration is logged and the corresponding
/// handler is dropped, leaving the daemon running. Hence the infallible
/// return type — sibling of the PS-48 pidfile/log warn-continue policy.
pub async fn install_signal_handlers(in_fg_mode: bool) -> ShutdownSignal {
    // SIGPIPE → ignore. tokio::signal doesn't expose SIG_IGN
    // directly, so use nix.
    // SAFETY: signal(SIGPIPE, SIG_IGN) is async-signal-safe and
    // disposition-only — no userspace handler installed.
    unsafe {
        if let Err(e) = signal(Signal::SIGPIPE, SigHandler::SigIgn) {
            tracing::error!(error = %e, "procserv-rs: unable to ignore SIGPIPE; continuing");
        }
    }

    // C `procServ.cc:502-503`: SIGXFSZ → SIG_IGN in the parent, in both
    // modes. The child then inherits it ignored (SIG_IGN survives
    // `execve`), which is why `child::restore_c_child_signal_environment`
    // must not reset it.
    // SAFETY: disposition-only (SIG_IGN); no userspace handler.
    unsafe {
        if let Err(e) = signal(Signal::SIGXFSZ, SigHandler::SigIgn) {
            tracing::error!(error = %e, "procserv-rs: unable to ignore SIGXFSZ; continuing");
        }
    }

    // C `procServ.cc:503-509`: in foreground mode both SIGINT and
    // SIGQUIT are SIG_IGN, so the console's ^C / ^\ belong to the
    // operator's session and never reach the supervisor as a shutdown.
    // Unchecked in C; logged-and-continued here, like every other
    // registration in this function.
    if in_fg_mode {
        for sig in [Signal::SIGINT, Signal::SIGQUIT] {
            // SAFETY: disposition-only (SIG_IGN); no userspace handler.
            unsafe {
                if let Err(e) = signal(sig, SigHandler::SigIgn) {
                    tracing::error!(error = %e, signal = ?sig, "procserv-rs: unable to ignore signal in foreground mode; continuing");
                }
            }
        }
    }

    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(error = %e, "procserv-rs: unable to register SIGTERM handler; graceful SIGTERM shutdown disabled");
            None
        }
    };
    // In foreground mode SIGINT stays ignored — registering the tokio
    // stream would re-arm it as a shutdown trigger and undo the SIG_IGN
    // above.
    let mut intr = if in_fg_mode {
        None
    } else {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(error = %e, "procserv-rs: unable to register SIGINT handler; graceful SIGINT shutdown disabled");
                None
            }
        }
    };

    let (tx, rx) = oneshot::channel::<ShutdownReason>();

    tokio::spawn(async move {
        let reason = tokio::select! {
            _ = recv_optional_signal(&mut term) => ShutdownReason::Terminate,
            _ = recv_optional_signal(&mut intr) => ShutdownReason::Interrupt,
        };
        let _ = tx.send(reason);
    });

    ShutdownSignal { rx }
}

/// Await an optional signal stream, parking forever when the stream is
/// absent. A `None` arises only when the handler failed to register: C's
/// `sigaction` calls are unchecked (`procServ.cc:496-509`), so a
/// registration failure must not abort the daemon — instead the matching
/// `select!` arm never fires and that handler is silently dropped, exactly
/// as C would run without a working handler.
pub(crate) async fn recv_optional_signal(s: &mut Option<tokio::signal::unix::Signal>) {
    match s {
        Some(sig) => {
            sig.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
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
    use super::*;

    // We don't unit-test fork_and_go (forking from cargo test is
    // hostile) or live signal *delivery* (process-wide state, and a
    // registration failure can't be forced deterministically — like
    // PS-32's fork path, this is covered by inspection). What we can
    // pin is the degradation contract introduced for PS-50.

    /// PS-50: a `None` signal stream — the state left when registration
    /// failed and we warned-and-continued — must park forever so its
    /// `select!` arm never fires. If it instead resolved, the supervisor
    /// would hot-loop `reopen_log()` (or the shutdown task would fire a
    /// spurious shutdown). The daemon must simply run without that handler.
    #[tokio::test]
    async fn recv_optional_signal_none_never_fires() {
        let mut absent: Option<tokio::signal::unix::Signal> = None;
        let parked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            recv_optional_signal(&mut absent),
        )
        .await;
        assert!(
            parked.is_err(),
            "an absent (failed-to-register) signal stream must never resolve"
        );
    }

    /// Query a signal's current disposition without changing it
    /// (`sigaction(sig, NULL, &old)`).
    fn disposition(sig: libc::c_int) -> libc::sighandler_t {
        // SAFETY: query-only — a null `act` leaves the disposition
        // untouched and fills `old`.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut old), 0);
            old.sa_sigaction
        }
    }

    /// Env var marking "this process is the isolated child `run_isolated`
    /// spawned; run the real test body instead of spawning another one."
    const ISOLATION_CHILD_MARKER: &str = "EPICS_TOOLS_RS_DAEMON_SIGNAL_TEST_CHILD";

    /// The tests below all mutate or inspect process-wide signal
    /// dispositions through [`install_signal_handlers`]'s raw `sigaction`
    /// calls, and each assumes it starts from that state fresh (e.g.
    /// `sigxfsz_is_ignored_in_daemon_mode`'s `SIG_DFL` precondition). That
    /// assumption is only true with one process per test — `cargo
    /// nextest` gives that, but plain `cargo test` runs every test in
    /// this binary as OS threads inside one shared process.
    ///
    /// Worse than an ordinary data race: `tokio::signal::unix::signal`
    /// (used for SIGINT in daemon mode) installs its OS-level handler via
    /// `signal-hook-registry`, which sets up its `sigaction` trampoline
    /// exactly ONCE per signal number for the life of the process and
    /// never re-installs or reverts it afterward (see its own
    /// `unregister` docs). Once a *different* test's raw
    /// `nix::sys::signal::signal(SIGINT, SigIgn)` (foreground mode) has
    /// stomped that trampoline in a shared process, no later
    /// `tokio::signal::unix::signal` call — nor a mutex, nor resetting
    /// the disposition by hand — brings it back: the registry believes
    /// its handler is already installed and won't touch `sigaction`
    /// again. Only a fresh process per test avoids this, so `run_isolated`
    /// re-execs this test binary for exactly one named test (`--exact`)
    /// in a child process and asserts it exited cleanly, reproducing
    /// nextest's process-per-test guarantee without depending on it being
    /// installed. `Command::spawn` (fork+exec) is used rather than a bare
    /// `fork()`, which would be unsafe here — this binary's other tests
    /// run concurrently on other OS threads, and forking a multithreaded
    /// process risks the child inheriting a lock (e.g. the allocator's)
    /// held by a thread that no longer exists to release it.
    fn run_isolated(test_name: &str, body: impl FnOnce()) {
        if std::env::var_os(ISOLATION_CHILD_MARKER).is_some() {
            body();
            return;
        }
        let exe = std::env::current_exe().expect("current test exe");
        let output = std::process::Command::new(exe)
            .args(["--exact", test_name])
            .env(ISOLATION_CHILD_MARKER, "1")
            .output()
            .expect("spawn isolated signal-disposition test process");
        assert!(
            output.status.success(),
            "{test_name} failed in its isolated child process ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
        );
    }

    /// R6-25 / C `procServ.cc:503-509`: foreground mode sets SIGINT and
    /// SIGQUIT to `SIG_IGN`. `Ctrl-C` and `Ctrl-\` at a `procserv-rs -f`
    /// prompt are the console client's keystrokes; they must not shut
    /// the supervisor down (SIGINT) or core-dump it (SIGQUIT's default).
    #[test]
    fn foreground_mode_ignores_sigint_and_sigquit() {
        run_isolated(
            "procserv::daemon::tests::foreground_mode_ignores_sigint_and_sigquit",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build isolated test runtime")
                    .block_on(async {
                        let _shutdown = install_signal_handlers(true).await;
                        assert_eq!(
                            disposition(libc::SIGINT),
                            libc::SIG_IGN,
                            "foreground mode must ignore SIGINT (C procServ.cc:504-505)"
                        );
                        assert_eq!(
                            disposition(libc::SIGQUIT),
                            libc::SIG_IGN,
                            "foreground mode must ignore SIGQUIT (C procServ.cc:506-507)"
                        );
                    });
            },
        );
    }

    /// R6-76 / C `procServ.cc:502-503`: `sigaction(SIGXFSZ, SIG_IGN)` runs
    /// in the parent unconditionally — no `inFgMode` gate. A write past
    /// `RLIMIT_FSIZE` (a `ulimit -f`-capped log) must fail with `EFBIG`,
    /// not kill the supervisor, and the child must inherit the ignored
    /// disposition through `execvp`.
    #[test]
    fn sigxfsz_is_ignored_in_daemon_mode() {
        run_isolated(
            "procserv::daemon::tests::sigxfsz_is_ignored_in_daemon_mode",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build isolated test runtime")
                    .block_on(async {
                        assert_eq!(
                            disposition(libc::SIGXFSZ),
                            libc::SIG_DFL,
                            "precondition: SIGXFSZ starts at its default disposition"
                        );
                        let _shutdown = install_signal_handlers(false).await;
                        assert_eq!(
                            disposition(libc::SIGXFSZ),
                            libc::SIG_IGN,
                            "SIGXFSZ must be ignored in the parent (C procServ.cc:502-503)"
                        );
                    });
            },
        );
    }

    /// Same set in foreground mode — the C call sits above the `inFgMode`
    /// block, so both modes must land on `SIG_IGN`.
    #[test]
    fn sigxfsz_is_ignored_in_foreground_mode() {
        run_isolated(
            "procserv::daemon::tests::sigxfsz_is_ignored_in_foreground_mode",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build isolated test runtime")
                    .block_on(async {
                        let _shutdown = install_signal_handlers(true).await;
                        assert_eq!(
                            disposition(libc::SIGXFSZ),
                            libc::SIG_IGN,
                            "SIGXFSZ must be ignored in foreground mode too"
                        );
                    });
            },
        );
    }

    /// The other side of the same C gate: with `inFgMode` false, C
    /// installs neither `SIG_IGN`, so SIGINT stays a shutdown trigger
    /// (tokio installs its own handler — the disposition is neither
    /// `SIG_IGN` nor `SIG_DFL`) and SIGQUIT keeps its default.
    #[test]
    fn daemon_mode_keeps_sigint_as_shutdown_and_sigquit_default() {
        run_isolated(
            "procserv::daemon::tests::daemon_mode_keeps_sigint_as_shutdown_and_sigquit_default",
            || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build isolated test runtime")
                    .block_on(async {
                        let _shutdown = install_signal_handlers(false).await;
                        let sigint = disposition(libc::SIGINT);
                        assert_ne!(
                            sigint,
                            libc::SIG_IGN,
                            "daemon mode must not ignore SIGINT — it is a shutdown trigger"
                        );
                        assert_ne!(
                            sigint,
                            libc::SIG_DFL,
                            "daemon mode installs a SIGINT handler for graceful shutdown"
                        );
                        assert_eq!(
                            disposition(libc::SIGQUIT),
                            libc::SIG_DFL,
                            "C leaves SIGQUIT alone outside foreground mode"
                        );
                    });
            },
        );
    }
}
