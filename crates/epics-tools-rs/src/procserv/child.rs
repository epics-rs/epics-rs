//! PTY-based child process management.
//!
//! Wraps `forkpty(3)` (via `nix::pty::forkpty`) to launch the
//! supervised child with its stdin/stdout/stderr connected to a
//! pseudo-terminal. The supervisor owns the master fd; we read its
//! output asynchronously via `tokio::io::unix::AsyncFd` and write
//! to it (filtered through `ignore_chars`) when forwarding party-line
//! input to the child stdin.
//!
//! Mirrors C `processClass` / `processFactory` (`processFactory.cc`):
//! - `forkpty` + `execvp`
//! - `setsid()` so signals to `-pid` reach the whole process group
//! - per-line PTY-master read with EIO/EOF → child-died detection
//! - `kill(-pid, sig)` for signal forwarding to the entire group
//! - SIGCHLD-style reap via blocking `waitpid` on a side task

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::pty::ForkptyResult;
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, chdir, execvp};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::procserv::error::{ProcServError, ProcServResult};

/// Faithful child-exit classification, mirroring C procServ's
/// `WIFEXITED` / `WIFSIGNALED` split in the SIGCHLD reaper
/// (`procServ.cc:794-805`). A signal death must NOT be collapsed into a
/// `128 + signo` exit code: C reports "killed by signal N" distinctly
/// from "Normal exit status = N", and `main` returns the `WEXITSTATUS`
/// (not `128+sig`) as procServ's own exit status (`procServ.cc:798,701`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    /// `WIFEXITED` — child exited normally with this status code.
    Exited(i32),
    /// `WIFSIGNALED` — child was terminated by this signal number.
    Signaled(i32),
    /// `waitpid` failed or returned an unhandled status (stopped/continued).
    Unknown,
}

/// Lifecycle event emitted by [`ChildHandle`] over its event channel.
#[derive(Debug)]
pub enum ChildEvent {
    /// PTY produced a chunk of output (child stdout/stderr).
    Output(Vec<u8>),
    /// Child process terminated. The supervisor consults the restart
    /// policy and either re-spawns or exits.
    Exited { exit: ChildExit },
}

/// Configuration for one child launch. Mirrors the subset of
/// [`crate::procserv::config::ChildConfig`] this module needs.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// The positional command — presented as argv[0] to the child, and
    /// the exec target unless [`Self::child_exec`] overrides it.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub ignore_chars: Vec<u8>,
    /// `RLIMIT_CORE` soft limit to set in the child before `exec`
    /// (`--coresize`). `None` ⇒ leave the inherited limit untouched.
    pub core_size: Option<u64>,
    /// Override executable to `exec` instead of [`Self::program`]
    /// (`--exec`). `None` ⇒ exec `program`. argv[0] stays `program`
    /// regardless, so a different binary runs under the original
    /// command line.
    pub child_exec: Option<PathBuf>,
}

/// Handle to a running child process. Cloning is cheap (Arcs inside).
#[derive(Clone)]
pub struct ChildHandle {
    pid: Pid,
    master: Arc<AsyncFd<OwnedFd>>,
    /// Filter applied to outbound bytes (matches C `--ignore` flag).
    ignore_chars: Arc<Vec<u8>>,
    alive: Arc<AtomicBool>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("pid", &self.pid.as_raw())
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish()
    }
}

impl ChildHandle {
    /// Spawn a new child via `forkpty` + `execvp`, returning the
    /// handle plus the receiver for [`ChildEvent`]s. The receiver is
    /// closed when the child exits and its PTY drains.
    ///
    /// Two side tasks are spawned per child:
    /// 1. **PTY reader** — reads from master fd, emits `Output` events
    /// 2. **Reaper** — blocking `waitpid` on a `spawn_blocking` thread,
    ///    emits the final `Exited` event and flips `alive` to false
    pub fn spawn(spec: &ChildSpec) -> ProcServResult<(Self, mpsc::Receiver<ChildEvent>)> {
        // SAFETY: forkpty is unsafe because between fork and exec we
        // may only call async-signal-safe functions. We do exactly
        // that: setsid + chdir (libc syscalls, both AS-safe) + execvp.
        let result = unsafe { nix::pty::forkpty(None, None) }
            .map_err(|e| ProcServError::Forkpty(e.to_string()))?;

        match result {
            ForkptyResult::Parent { child, master } => {
                let alive = Arc::new(AtomicBool::new(true));
                set_nonblocking(&master)
                    .map_err(|e| ProcServError::Forkpty(format!("set O_NONBLOCK: {e}")))?;
                let master_fd = Arc::new(
                    AsyncFd::new(master)
                        .map_err(|e| ProcServError::Forkpty(format!("AsyncFd: {e}")))?,
                );

                let (tx, rx) = mpsc::channel::<ChildEvent>(64);

                // Reader task: pump PTY-master → ChildEvent::Output.
                spawn_reader(master_fd.clone(), tx.clone());

                // Reaper task: blocking waitpid → ChildEvent::Exited.
                spawn_reaper(child, alive.clone(), tx);

                Ok((
                    Self {
                        pid: child,
                        master: master_fd,
                        ignore_chars: Arc::new(spec.ignore_chars.clone()),
                        alive,
                    },
                    rx,
                ))
            }
            ForkptyResult::Child => {
                // We're in the child. Exec the target program.
                // `in_child_setup_and_exec` returns `!` — either
                // execvp succeeds (we never return) or it logs the
                // error and `process::exit`s.
                in_child_setup_and_exec(spec);
            }
        }
    }

    /// Write bytes to the child's stdin (PTY-master write). Called
    /// by the supervisor when a non-readonly client's input is
    /// being forwarded to the child via the party-line.
    ///
    /// `ignore_chars` from [`ChildSpec`] are filtered out before the
    /// write — matches C procServ's `--ignore` flag handled in
    /// `processClass::Send`.
    pub async fn write_stdin(&self, bytes: &[u8]) -> ProcServResult<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(ProcServError::ChildExited(None));
        }

        let filtered: Vec<u8> = if self.ignore_chars.is_empty() {
            bytes.to_vec()
        } else {
            bytes
                .iter()
                .copied()
                .filter(|b| !self.ignore_chars.contains(b))
                .collect()
        };
        if filtered.is_empty() {
            return Ok(());
        }

        let mut written = 0;
        while written < filtered.len() {
            let mut guard = self.master.writable().await.map_err(ProcServError::Io)?;
            let raw = self.master.as_ref().as_raw_fd();
            let result = guard.try_io(|_| {
                let n = unsafe {
                    libc::write(
                        raw,
                        filtered[written..].as_ptr() as *const libc::c_void,
                        filtered.len() - written,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match result {
                Ok(Ok(n)) => written += n,
                Ok(Err(e)) => return Err(ProcServError::Io(e)),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Send a signal to the child process group. Negative pid means
    /// "all processes in pgid", which is what we want — `setsid`
    /// makes the child its own group leader so we can signal the
    /// whole tree of grandchildren too.
    pub fn signal(&self, signo: i32) -> ProcServResult<()> {
        let sig = Signal::try_from(signo)
            .map_err(|e| ProcServError::Config(format!("invalid signal {signo}: {e}")))?;
        // Negative pid → process group.
        let pgid = Pid::from_raw(-self.pid.as_raw());
        nix::sys::signal::kill(pgid, sig)
            .map_err(|e| ProcServError::Io(io::Error::other(e.to_string())))?;
        Ok(())
    }

    /// Whether the child is currently alive. The menu-key dispatch
    /// (`Action::evaluate(child_alive=…)`) consults this each
    /// keystroke.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// PID of the child (for info-file rendering).
    pub fn pid(&self) -> i32 {
        self.pid.as_raw()
    }
}

/// In-child path of `forkpty` — optional chdir, then `execvp` the
/// target program. Never returns on success; on failure prints to
/// stderr (which goes back through the PTY to the parent) and exits
/// with status 126 / 127.
///
/// Note: `forkpty(3)` already calls `setsid()` internally and
/// connects the slave fd as the controlling terminal, so we MUST
/// NOT call `setsid` here again — it would return `EPERM` because
/// we're already a session leader.
fn in_child_setup_and_exec(spec: &ChildSpec) -> ! {
    // Apply the core-dump rlimit before chdir/exec, matching C's order
    // (processFactory.cc:206-210): `getrlimit` to keep the hard limit,
    // set only the soft limit (`rlim_cur`) to `coreSize`. Best-effort —
    // C ignores the setrlimit return too. getrlimit/setrlimit are
    // async-signal-safe, so this is safe in the post-forkpty child.
    if let Some(core) = spec.core_size
        && let Ok((_, hard)) = getrlimit(Resource::RLIMIT_CORE)
    {
        let _ = setrlimit(Resource::RLIMIT_CORE, core, hard);
    }

    if let Some(ref cwd) = spec.cwd {
        let c_cwd = match CString::new(cwd.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("procserv child: invalid chdir path");
                std::process::exit(126);
            }
        };
        if let Err(e) = chdir(c_cwd.as_c_str()) {
            eprintln!("procserv child: chdir to {} failed: {e}", cwd.display());
            std::process::exit(126);
        }
    }

    // argv[0] presented to the child is always the positional command
    // (`spec.program`), so a `--exec` override runs a different binary
    // under the original command line. C `childExec` (procServ.cc:62,
    // 459-462); C's argv[0]-is-the-prior-token quirk is not reproduced.
    let arg0 = match CString::new(spec.program.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("procserv child: program name contains NUL");
            std::process::exit(126);
        }
    };
    // The binary actually exec'd: the `--exec` override if set, else the
    // command itself. `execvp` PATH-searches a slashless name, like C.
    let exec_target = match &spec.child_exec {
        Some(exe) => match CString::new(exe.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("procserv child: exec path contains NUL");
                std::process::exit(126);
            }
        },
        None => arg0.clone(),
    };
    let mut argv: Vec<CString> = Vec::with_capacity(1 + spec.args.len());
    argv.push(arg0);
    for a in &spec.args {
        match CString::new(a.as_bytes()) {
            Ok(c) => argv.push(c),
            Err(_) => {
                eprintln!("procserv child: argument contains NUL: {a:?}");
                std::process::exit(126);
            }
        }
    }

    let argv_refs: Vec<&std::ffi::CStr> = argv.iter().map(|c| c.as_c_str()).collect();
    match execvp(exec_target.as_c_str(), &argv_refs) {
        Ok(infallible) => match infallible {},
        Err(e) => {
            eprintln!(
                "procserv child: execvp({}) failed: {e}",
                spec.child_exec.as_ref().unwrap_or(&spec.program).display()
            );
            std::process::exit(127);
        }
    }
}

/// Set `O_NONBLOCK` on a borrowed fd. Required before wrapping in
/// `AsyncFd` — otherwise `try_io`'s read syscall blocks indefinitely
/// on no-data instead of returning `EAGAIN` for the runtime to wait.
fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reader task: pumps PTY-master into [`ChildEvent::Output`]
/// messages until EOF / EIO (which is what we get when the child
/// exits and its slave side closes).
fn spawn_reader(master: Arc<AsyncFd<OwnedFd>>, tx: mpsc::Sender<ChildEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        let raw: RawFd = master.as_ref().as_raw_fd();
        loop {
            let mut guard = match master.readable().await {
                Ok(g) => g,
                Err(e) => {
                    tracing::debug!(error = %e, "procserv child PTY readable() ended");
                    break;
                }
            };
            match guard.try_io(|_| {
                let n =
                    unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    // Send a copy; the buffer is reused next iteration.
                    let chunk = buf[..n].to_vec();
                    if tx.send(ChildEvent::Output(chunk)).await.is_err() {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    // EIO commonly means the slave end closed — child died.
                    if e.raw_os_error() == Some(libc::EIO) {
                        tracing::debug!("procserv child PTY EIO (slave closed)");
                    } else {
                        tracing::debug!(error = %e, "procserv child PTY read error");
                    }
                    break;
                }
                Err(_would_block) => continue,
            }
        }
    })
}

/// Reaper task: blocking `waitpid` on a thread; emits the final
/// [`ChildEvent::Exited`] then closes the channel.
fn spawn_reaper(pid: Pid, alive: Arc<AtomicBool>, tx: mpsc::Sender<ChildEvent>) -> JoinHandle<()> {
    tokio::task::spawn(async move {
        let res = tokio::task::spawn_blocking(move || waitpid(pid, None))
            .await
            .ok();
        // Preserve C's WIFEXITED/WIFSIGNALED distinction (procServ.cc:794-805):
        // a signal death is reported as such, never folded into 128+sig.
        let exit = match res {
            Some(Ok(WaitStatus::Exited(_, code))) => ChildExit::Exited(code),
            Some(Ok(WaitStatus::Signaled(_, sig, _))) => ChildExit::Signaled(sig as i32),
            _ => ChildExit::Unknown,
        };
        alive.store(false, Ordering::Release);
        let _ = tx.send(ChildEvent::Exited { exit }).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep};

    /// Drain `rx` until the channel closes or `deadline` elapses,
    /// collecting all output. We deliberately do NOT break on
    /// `Exited` — the reader and reaper run in parallel, so a final
    /// chunk of PTY output can arrive after the exit signal.
    /// Channel closure (`None`) is the authoritative end-of-life
    /// because both reader and reaper drop their tx clones before
    /// returning.
    async fn drain_until_closed(
        rx: &mut mpsc::Receiver<ChildEvent>,
        deadline: tokio::time::Instant,
    ) -> (Vec<u8>, bool) {
        let mut output = Vec::new();
        let mut exited = false;
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ChildEvent::Output(b)) => output.extend_from_slice(&b),
                    Some(ChildEvent::Exited { .. }) => exited = true,
                    None => break,
                },
                _ = sleep(Duration::from_millis(50)) => {}
            }
        }
        (output, exited)
    }

    #[tokio::test]
    async fn spawn_echo_child_yields_output_and_exits() {
        let spec = ChildSpec {
            program: PathBuf::from("/bin/echo"),
            args: vec!["hello procserv".into()],
            cwd: None,
            ignore_chars: Vec::new(),
            core_size: None,
            child_exec: None,
        };
        let (handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let (output, exited) = drain_until_closed(&mut rx, deadline).await;

        assert!(exited, "child should have exited");
        assert!(!handle.is_alive(), "alive flag should flip false");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("hello procserv"), "got: {text:?}");
    }

    /// Drain until the channel closes, returning the final exit
    /// classification (the last `Exited` seen). Mirrors
    /// [`drain_until_closed`] but keeps the [`ChildExit`] value.
    async fn drain_for_exit(
        rx: &mut mpsc::Receiver<ChildEvent>,
        deadline: tokio::time::Instant,
    ) -> Option<ChildExit> {
        let mut exit = None;
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ChildEvent::Output(_)) => {}
                    Some(ChildEvent::Exited { exit: e }) => exit = Some(e),
                    None => break,
                },
                _ = sleep(Duration::from_millis(50)) => {}
            }
        }
        exit
    }

    /// A normal exit is classified `Exited(code)` carrying the real
    /// status code — `/bin/sh -c 'exit 3'` reports `Exited(3)`, never
    /// a signal (C WIFEXITED, procServ.cc:794-798).
    #[tokio::test]
    async fn normal_exit_reports_exit_code() {
        let spec = ChildSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 3".into()],
            cwd: None,
            ignore_chars: Vec::new(),
            core_size: None,
            child_exec: None,
        };
        let (_handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let exit = drain_for_exit(&mut rx, deadline).await;
        assert_eq!(exit, Some(ChildExit::Exited(3)), "got: {exit:?}");
    }

    /// A signal death is classified `Signaled(sig)` — NOT folded into a
    /// `128 + sig` exit code (C WIFSIGNALED, procServ.cc:801-805).
    /// SIGKILL (9) a `sleep` and confirm the reaper reports `Signaled(9)`.
    #[tokio::test]
    async fn signal_death_reports_signal_not_128_plus_sig() {
        let spec = ChildSpec {
            program: PathBuf::from("/bin/sleep"),
            args: vec!["30".into()],
            cwd: None,
            ignore_chars: Vec::new(),
            core_size: None,
            child_exec: None,
        };
        let (handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");
        // Let the child reach its sleep, then SIGKILL its group.
        sleep(Duration::from_millis(150)).await;
        handle.signal(9).expect("signal");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let exit = drain_for_exit(&mut rx, deadline).await;
        assert_eq!(exit, Some(ChildExit::Signaled(9)), "got: {exit:?}");
    }

    #[tokio::test]
    async fn write_stdin_filters_ignore_chars() {
        // `cat` echoes stdin to stdout; we'll feed it bytes and
        // verify the ignore filter strips them before the write.
        let spec = ChildSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![],
            cwd: None,
            ignore_chars: vec![b'X'],
            core_size: None,
            child_exec: None,
        };
        let (handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");

        // Wait briefly for the PTY to settle and for cat to be in
        // its read loop. Without this, the first write can race the
        // exec.
        sleep(Duration::from_millis(150)).await;
        handle.write_stdin(b"abXXcd\n").await.expect("write");
        // Give cat a moment to echo before sending EOF.
        sleep(Duration::from_millis(150)).await;
        // Send EOF (Ctrl-D = 0x04) so cat exits cleanly.
        handle.write_stdin(&[0x04]).await.ok();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let (output, _) = drain_until_closed(&mut rx, deadline).await;

        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("abcd"), "filter stripped X's, got: {text:?}");
        assert!(
            !text.contains('X'),
            "X bytes should not appear, got: {text:?}"
        );
    }

    /// `core_size` must set the child's `RLIMIT_CORE` soft limit before
    /// exec (C `setCoreSize`, processFactory.cc:206-210). Verify via
    /// `ulimit -c`, which prints the soft limit in 512- or 1024-byte
    /// blocks (platform-dependent). When the inherited hard limit allows
    /// a distinctive 1 MiB soft cap, assert the reported block count
    /// matches; otherwise just assert the child still execs with the
    /// limit applied (no clean distinctive value is settable).
    #[tokio::test]
    async fn core_size_applies_rlimit_core_to_child() {
        let (_soft, hard) = getrlimit(Resource::RLIMIT_CORE).expect("getrlimit");
        let one_mib: u64 = 1024 * 1024;
        let strong = hard >= one_mib; // can set a clean, distinctive cap
        let target = if strong { one_mib } else { hard };

        let spec = ChildSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "ulimit -c".into()],
            cwd: None,
            ignore_chars: Vec::new(),
            core_size: Some(target),
            child_exec: None,
        };
        let (_handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let (output, _exited) = drain_until_closed(&mut rx, deadline).await;
        let text = String::from_utf8_lossy(&output);
        let reported = text
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");

        if strong {
            // 1 MiB ÷ 512 = 2048 blocks; ÷ 1024 = 1024 blocks.
            let blocks_512 = (target / 512).to_string();
            let blocks_1024 = (target / 1024).to_string();
            assert!(
                reported == blocks_512 || reported == blocks_1024,
                "child ulimit -c should reflect the 1 MiB core_size \
                 ({blocks_512} or {blocks_1024} blocks); got {reported:?}"
            );
        } else {
            // Hard limit too small for a distinctive cap — just prove the
            // child execed and produced ulimit output with the limit set.
            assert!(
                !reported.is_empty(),
                "child should exec and report its core limit; got {text:?}"
            );
        }
    }

    /// `child_exec` (`--exec`) runs a different binary than the positional
    /// command while presenting argv[0] = the command (C `childExec`,
    /// procServ.cc:459-462). `program` is a bare display name (not a real
    /// binary) and `child_exec` is `/bin/sh`: the shell must run (proving
    /// the exec target is `/bin/sh`, not the name) and `echo $0` must print
    /// the display name (proving argv[0]).
    #[tokio::test]
    async fn child_exec_runs_override_binary_with_command_as_argv0() {
        let spec = ChildSpec {
            program: PathBuf::from("ioc-display-name"),
            args: vec!["-c".into(), "echo $0".into()],
            cwd: None,
            ignore_chars: Vec::new(),
            core_size: None,
            child_exec: Some(PathBuf::from("/bin/sh")),
        };
        let (_handle, mut rx) = ChildHandle::spawn(&spec).expect("spawn");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let (output, _exited) = drain_until_closed(&mut rx, deadline).await;
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("ioc-display-name"),
            "child_exec should run /bin/sh with argv[0]=program; got {text:?}"
        );
    }
}
