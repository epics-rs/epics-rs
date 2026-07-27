//! Top-level configuration handed to [`super::ProcServ`].
//!
//! Mirrors C procServ's command-line flag set 1:1 so existing
//! deployments can switch to `procserv-rs` with the same wrapper
//! scripts. Defaults match C procServ's defaults.

use std::path::PathBuf;
use std::time::Duration;

use crate::procserv::endpoint::Endpoint;
use crate::procserv::restart::{RestartMode, RestartPolicy};

/// Listen-side configuration. C procServ collects control endpoints into
/// a `ctlSpecs` vector built from repeatable `-P` plus the legacy
/// positional port (`procServ.cc:211,386,453`), and a single optional log
/// endpoint from `-l`; each spec is parsed by `acceptFactory`. The Rust
/// port mirrors that shape: a vector of control endpoints (read/write) and
/// one optional log endpoint (read-only).
#[derive(Debug, Clone, Default)]
pub struct ListenConfig {
    /// Control endpoints (read/write). Each is a fully-resolved
    /// [`Endpoint`] — TCP (bare port or `A.B.C.D:port` interface bind) or
    /// UNIX (filesystem, access-controlled, or abstract). At least one is
    /// required (C's arg-count check, `procServ.cc:420`). C `ctlSpecs`.
    pub control: Vec<Endpoint>,
    /// Read-only viewer/log endpoint (`-l` / `--logport`). Clients here
    /// receive all output but their input is discarded — C procServ's log
    /// listener, created with `readonly=true` (`procServ.cc:533`,
    /// `acceptFactory.cc:395`). Its localhost-vs-any decision is C's
    /// `logPortLocal`: all interfaces by default, localhost under
    /// `--restrict` (the inverse of the control port's localhost default +
    /// `--allow`). `None` disables it.
    pub log: Option<Endpoint>,
}

/// Per-input-key bindings. Matches C procServ's `restartChar`
/// (toggle restart mode), `killChar` (send signal to child),
/// `restartChar` for kill-then-restart, `quitChar` for "shut down
/// procServ entirely", and `logoutChar` for "disconnect this client
/// only".
///
/// Each is `Option<u8>` because C procServ accepts the literal byte
/// 0 to disable any given binding individually.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyBindings {
    /// Send signal to child (default `Ctrl-X` = `0x18`).
    pub kill: Option<u8>,
    /// Toggle restart mode (default `Ctrl-T` = `0x14`).
    pub toggle_restart: Option<u8>,
    /// Restart child once after manual kill (default `Ctrl-R` = `0x12`).
    pub restart: Option<u8>,
    /// Shut down procserv entirely. C `quitChar` = `^Q` (`0x11`),
    /// always enabled and never exposed on the CLI (procServ.cc:69);
    /// it fires only while the child is shut down.
    pub quit: Option<u8>,
    /// Disconnect this client only. C `logoutChar` starts at `0x00`
    /// (disabled) and is enabled only by `--logoutcmd` (procServ.cc:70);
    /// `^]` (`0x1d`) is merely the value the docs suggest, not a default.
    pub logout: Option<u8>,
}

/// Configuration for the supervised child process.
#[derive(Debug, Clone)]
pub struct ChildConfig {
    /// Executable name (display only; goes into welcome banner).
    pub name: String,
    /// The positional command. Used as `argv[0]` presented to the child,
    /// and as the exec target unless [`Self::child_exec`] overrides it.
    pub program: PathBuf,
    /// Remaining argv.
    pub args: Vec<String>,
    /// Override executable to `exec` instead of [`Self::program`]
    /// (`--exec`). `None` ⇒ exec `program` itself (the default). When
    /// set, the child still sees `argv[0]` = `program`, so it runs a
    /// different binary while presenting the original command line —
    /// C `childExec` (`procServ.cc:62,295-296,459-462`). C's `argv[0]`
    /// quirk (the token before the command) is not reproduced; `argv[0]`
    /// is the command's arg0, matching the documented intent.
    pub child_exec: Option<PathBuf>,
    /// Working directory for the child (optional `--chdir`).
    pub cwd: Option<PathBuf>,
    /// Signal sent on `kill` keybinding. C procServ defaults to
    /// `SIGKILL`; many sites override with `SIGINT` for graceful IOC
    /// shutdown.
    pub kill_signal: i32,
    /// Characters to discard from PTY-master writes (`--ignore`).
    /// Empty = no filtering.
    pub ignore_chars: Vec<u8>,
    /// Core-dump size limit (`RLIMIT_CORE` soft limit) applied to the
    /// child before `exec` (`--coresize`). `None` leaves the inherited
    /// limit untouched, matching C's `setCoreSize` flag which is only
    /// honored for `coresize >= 0` (`procServ.cc:279-285`,
    /// `processFactory.cc:206-210`).
    pub core_size: Option<u64>,
}

/// Sidecar/log configuration.
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    /// Path to the log file (`--logfile`). `None` disables logging.
    pub log_path: Option<PathBuf>,
    /// Path to write the supervisor's PID (`--pidfile`).
    pub pid_path: Option<PathBuf>,
    /// Path to write a status info file consumed by `manage-procs`.
    pub info_path: Option<PathBuf>,
    /// Whether to prefix each log line with a timestamp. C procServ's
    /// `stampLog` (`procServ.cc:82`), default `false`: the log is written
    /// verbatim (`procServ.cc:744`) unless `--logstamp` is given, which
    /// sets `stampLog = true` (`procServ.cc:307-311`). When `false`,
    /// [`Self::stamp_format`] is unused and the log is byte-identical to
    /// the child's output.
    pub stamp_log: bool,
    /// Human-facing time format for banner lines (e.g. "server started
    /// at"). C procServ's `timeFormat` (`procServ.cc:80`, default `%c`),
    /// applied raw with no surrounding punctuation.
    pub time_format: String,
    /// Per-line LOG prefix format, applied verbatim to each new line.
    /// C procServ's `stampFormat` (`procServ.cc:83`), written raw at
    /// `procServ.cc:721`. Distinct from [`Self::time_format`]: C's default
    /// `stampFormat` is `"[" + timeFormat + "] "` (`procServ.cc:464-468`)
    /// — the brackets live in this format string, not in the writer, so a
    /// caller can supply an un-bracketed stamp and have it honored
    /// verbatim (C's `--logstamp <fmt>`, `procServ.cc:308-310`).
    pub stamp_format: String,
}

/// Full procserv configuration.
#[derive(Debug, Clone)]
pub struct ProcServConfig {
    /// Foreground mode. When `false`, [`super::daemon::fork_and_go`]
    /// runs and the parent exits.
    pub foreground: bool,
    pub listen: ListenConfig,
    pub keys: KeyBindings,
    pub child: ChildConfig,
    pub logging: LoggingConfig,
    pub restart: RestartPolicy,
    pub restart_mode: RestartMode,
    /// Hold-off between child restarts (matches C `holdoffTime`).
    pub holdoff: Duration,
    /// Wait for first manual restart command before launching the
    /// child (`--wait`).
    pub wait_for_manual_start: bool,
}

impl ProcServConfig {
    /// Validate the config — bail at construction time rather than
    /// surfacing an error mid-run.
    pub fn validate(&self) -> Result<(), String> {
        if self.listen.control.is_empty() {
            return Err("at least one control endpoint is required".into());
        }
        if self.child.program.as_os_str().is_empty() {
            return Err("child.program is required".into());
        }
        Ok(())
    }
}
