//! Side-car file management — log file, info file, pid file, env vars.
//!
//! Mirrors C `openLogFile()`, `writeInfoFile()`, `writePidFile()`,
//! and `setEnvVar()` from `procServ.cc`. These exist to support the
//! `procServUtils/manage-procs` tooling, which inspects pid + info
//! files in a known directory to enumerate / attach / restart
//! procserv instances.
//!
//! The info file and the `PROCSERV_INFO` env var use the two distinct
//! formats `manage-procs` parses (`manage.py:38-44`), NOT a shared
//! `KEY=value` form:
//!
//! - info file (`writeInfoFile`, `procServ.cc:935-941`):
//!   `pid:<supervisor-pid>\n` followed by one `tcp:<ip>:<port>\n` /
//!   `unix:<path>\n` line per listener (`writeAddress`,
//!   `acceptFactory.cc:45-49,80-85`).
//! - `PROCSERV_INFO` env (`setEnvVar`, `procServ.cc:943-953`):
//!   `PID=<supervisor-pid>;CTL=tcp:<ip>:<port>;LOG=tcp:<ip>:<port>` with
//!   `CTL=`/`LOG=` per listener (`writeAddressEnv`,
//!   `acceptFactory.cc:52-61,88-98`) and the trailing `;` stripped.

use std::path::{Path, PathBuf};

use chrono::Local;
use parking_lot::Mutex as SyncMutex;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;

use crate::procserv::config::ListenConfig;
use crate::procserv::error::{ProcServError, ProcServResult};

/// Prefix every new line in `chunk` with `stamp`, tracking mid-line
/// state across calls via `in_line`. This is the one stamp-at-newline
/// algorithm C applies in two places — the log file write
/// (`procServ.cc:732-744`) and each logger client's stream
/// (`clientItem::Send`, `clientFactory.cc:264-276`) — each with its own
/// `_log_stamp_sent` flag. A stamp is emitted only at the start of a
/// line, so a chunk that does not end in `\n` leaves `*in_line == true`
/// and the next chunk continues the line without a fresh stamp.
pub(crate) fn stamp_lines(chunk: &[u8], stamp: &[u8], in_line: &mut bool) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(chunk.len() + stamp.len());
    let mut prev = 0usize;
    for (i, &b) in chunk.iter().enumerate() {
        if !*in_line {
            buf.extend_from_slice(stamp);
            *in_line = true;
        }
        if b == b'\n' {
            buf.extend_from_slice(&chunk[prev..=i]);
            prev = i + 1;
            *in_line = false;
        }
    }
    if prev < chunk.len() {
        buf.extend_from_slice(&chunk[prev..]);
    }
    buf
}

/// Per-line writer to the supervisor log. Wraps a file with timestamp
/// prefixing — every line emitted by the child PTY is prefixed with
/// the configured timestamp format. Multiple writers are serialized
/// via a parking_lot mutex around the file handle, but the typical
/// case is single-supervisor → single-log so contention is nil.
pub struct LogFile {
    /// Async mutex because the file write is held across `.await`.
    file: AsyncMutex<File>,
    /// Path the log was opened from, kept so [`Self::reopen`] (the
    /// SIGHUP/logrotate path) can re-open the same target.
    path: PathBuf,
    /// Whether to prefix each line with a timestamp. C `stampLog`
    /// (`procServ.cc:82`), default off: when `false` the chunk is written
    /// verbatim (`procServ.cc:744`) and [`Self::stamp_format`] is unused.
    stamp_log: bool,
    /// Per-line stamp format, applied RAW (C `stampFormat`,
    /// `procServ.cc:721`) when [`Self::stamp_log`] is set. Any
    /// bracketing/separator is part of this string, not added here — C's
    /// default `"[" + timeFormat + "] "` is just the default value,
    /// overridable verbatim.
    stamp_format: String,
    /// Tracks whether we're mid-line (no newline since last write).
    /// Matches the C `_log_stamp_sent` per-connection flag at
    /// clientFactory.cc:138 — a stamp only fires at the start of
    /// each new line, even when the PTY writes partial chunks.
    /// Sync mutex (parking_lot) because the critical section is
    /// pure CPU — no .await held while inspecting / mutating.
    in_line: SyncMutex<bool>,
}

impl LogFile {
    /// Open / create the log at `path` in append mode. Errors if the
    /// path's parent directory doesn't exist (we don't `mkdir -p`;
    /// matches C procServ which expects the operator to set up the
    /// directory).
    pub async fn open(
        path: &Path,
        stamp_log: bool,
        stamp_format: impl Into<String>,
    ) -> ProcServResult<Self> {
        let file = Self::open_handle(path).await?;
        Ok(Self {
            file: AsyncMutex::new(file),
            path: path.to_path_buf(),
            stamp_log,
            stamp_format: stamp_format.into(),
            in_line: SyncMutex::new(false),
        })
    }

    /// Open (create + append) a handle to `path`. Shared by [`Self::open`]
    /// and [`Self::reopen`].
    async fn open_handle(path: &Path) -> ProcServResult<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(ProcServError::Io)
    }

    /// Re-open the log file in place, replacing the current handle.
    ///
    /// This is the SIGHUP / logrotate path: C procServ's `OnSigHup`
    /// raises a flag the main loop turns into `openLogFile()`, which
    /// closes the old fd and re-opens the configured path
    /// (`procServ.cc:641-645`, `915-933`). After `logrotate` has
    /// renamed the live file out from under us, the next write must go
    /// to a freshly-created file at the original path, not the renamed
    /// inode the old handle still points at. Resets the mid-line state
    /// so the new file starts with a timestamp on its first line.
    pub async fn reopen(&self) -> ProcServResult<()> {
        let fresh = Self::open_handle(&self.path).await?;
        *self.file.lock().await = fresh;
        *self.in_line.lock() = false;
        Ok(())
    }

    /// Append a chunk of PTY output to the log, prefixing each new
    /// line with a timestamp. The chunk may contain zero or more
    /// `\n`s; partial lines are appended without a stamp until the
    /// next newline.
    pub async fn write_chunk(&self, chunk: &[u8]) -> ProcServResult<()> {
        // C default `stampLog == false`: write the child's bytes verbatim,
        // no per-line timestamp (`procServ.cc:744`). The log is then
        // byte-identical to the child output.
        if !self.stamp_log {
            let mut file = self.file.lock().await;
            file.write_all(chunk).await.map_err(ProcServError::Io)?;
            file.flush().await.map_err(ProcServError::Io)?;
            return Ok(());
        }

        // Build the output buffer inside an inner block so the
        // parking_lot guard is unambiguously dropped before the
        // first `.await`. parking_lot's `MutexGuard` is `!Send`, so
        // a guard that lingers in scope across an await poisons the
        // outer future's `Send` bound — the supervisor's `tokio::spawn`
        // would refuse to schedule it.
        let out: Vec<u8> = {
            let stamp = self.format_stamp();
            let mut in_line = self.in_line.lock();
            stamp_lines(chunk, stamp.as_bytes(), &mut in_line)
        }; // in_line guard dropped here

        // Hold file lock across the IO; tokio mutex serializes
        // concurrent writers without blocking other tasks.
        let mut file = self.file.lock().await;
        file.write_all(&out).await.map_err(ProcServError::Io)?;
        file.flush().await.map_err(ProcServError::Io)?;
        Ok(())
    }

    fn format_stamp(&self) -> String {
        // Apply the stamp format RAW — C writes `strftime(stampFormat)`
        // verbatim (procServ.cc:721). The default `stamp_format` already
        // carries its own `"[" … "] "` bracketing (C procServ.cc:464-468),
        // so the writer must not add any of its own or a caller-supplied
        // un-bracketed format could never be honored.
        Local::now().format(&self.stamp_format).to_string()
    }
}

/// Write the supervisor's pid to the configured pid file.
///
/// Atomic via tmp-file + rename so concurrent readers (e.g.
/// `manage-procs status`) never observe a partial write.
pub fn write_pid_file(path: &Path, pid: i32) -> ProcServResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("procserv.pid")
    ));
    std::fs::write(&tmp, format!("{pid}\n")).map_err(ProcServError::Io)?;
    std::fs::rename(&tmp, path).map_err(ProcServError::Io)?;
    Ok(())
}

/// Best-effort delete on graceful shutdown. Errors are logged and
/// swallowed — there's nothing we can do about a missing pid file at
/// shutdown anyway.
pub fn remove_pid_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(path = %path.display(), error = %e, "procserv-rs: failed to remove pid file");
    }
}

/// Status info file (C `writeInfoFile`, `procServ.cc:935-941`), the form
/// `manage-procs` parses (`manage.py:38-44`):
///
/// ```text
/// pid:NNNN
/// tcp:127.0.0.1:5000
/// unix:/run/ioc.sock
/// ```
///
/// Atomic via tmp+rename.
pub fn write_info_file(path: &Path, info: &InfoSnapshot) -> ProcServResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("procserv.info")
    ));
    let body = render_info_file(info);
    std::fs::write(&tmp, body).map_err(ProcServError::Io)?;
    std::fs::rename(&tmp, path).map_err(ProcServError::Io)?;
    Ok(())
}

/// One listening endpoint, as procServ writes it (`writeAddress` /
/// `writeAddressEnv`, `acceptFactory.cc:45-99`).
#[derive(Debug, Clone)]
pub struct ListenAddress {
    /// `true` ⇒ read-only log/viewer port (env prefix `LOG=`); `false` ⇒
    /// control port (`CTL=`). The info file does not distinguish them.
    pub readonly: bool,
    /// Address token: `tcp:<ip>:<port>` or `unix:<path>` / `unix:@<abstract>`.
    pub addr: String,
}

impl ListenAddress {
    /// `tcp:<ip>:<port>` — C `inet_ntop` + `ntohs(port)`, bare (no `[]`),
    /// `acceptFactory.cc:45-49,52-61`.
    pub fn tcp(addr: std::net::SocketAddr, readonly: bool) -> Self {
        Self {
            readonly,
            addr: format!("tcp:{}:{}", addr.ip(), addr.port()),
        }
    }

    /// `unix:<path>` — filesystem socket (`acceptFactory.cc:80-85`). The
    /// `unix:@<abstract>` form is not produced; the Rust port binds only
    /// filesystem sockets.
    pub fn unix(path: &Path, readonly: bool) -> Self {
        Self {
            readonly,
            addr: format!("unix:{}", path.display()),
        }
    }
}

/// Snapshot of supervisor identity + listening addresses, serialized into
/// the info file and the `PROCSERV_INFO` env var. Both are fixed for the
/// supervisor's lifetime (procServ writes them once at startup,
/// `procServ.cc:560-563`).
#[derive(Debug, Clone)]
pub struct InfoSnapshot {
    /// procServ supervisor pid — C `getpid()` (`procServ.cc:938,946`), the
    /// pid `manage-procs` probes for liveness, NOT the child IOC pid.
    pub procserv_pid: i32,
    /// Listening endpoints, in C `connectionItem::head` iteration order.
    pub addresses: Vec<ListenAddress>,
}

/// Render the info-file body (C `writeInfoFile`, `procServ.cc:935-941`):
/// `pid:` line then one address line per listener.
pub fn render_info_file(info: &InfoSnapshot) -> String {
    let mut out = format!("pid:{}\n", info.procserv_pid);
    for a in &info.addresses {
        out.push_str(&a.addr);
        out.push('\n');
    }
    out
}

/// Render the `PROCSERV_INFO` env value (C `setEnvVar`,
/// `procServ.cc:943-953`): `PID=<pid>;` then `CTL=`/`LOG=` per listener,
/// with the trailing `;` stripped.
pub fn render_procserv_info_env(info: &InfoSnapshot) -> String {
    let mut out = format!("PID={};", info.procserv_pid);
    for a in &info.addresses {
        out.push_str(if a.readonly { "LOG=" } else { "CTL=" });
        out.push_str(&a.addr);
        out.push(';');
    }
    // C `env_str.substr(0, env_str.size()-1)` strips the final ';'.
    if out.ends_with(';') {
        out.pop();
    }
    out
}

/// Build the listener address list in C `connectionItem::head` iteration
/// order. C prepends each acceptItem on creation (`procServ.cc:824-832`) and
/// creates the control listeners before the log listener
/// (`procServ.cc:515-534`), so head order is the reverse: the log port first,
/// then the control listeners. The Rust supervisor creates control TCP,
/// control UNIX, then log (`supervisor.rs:178-206`); reversed, that is log,
/// UNIX, control TCP. `manage-procs` is order-independent (`manage.py:68`
/// joins all ports), so this ordering is functional parity for any
/// combination and byte-exact for the common control-only / control+log
/// cases.
pub fn listen_addresses(listen: &ListenConfig) -> Vec<ListenAddress> {
    let mut out = Vec::new();
    if let Some(addr) = listen.log_bind {
        out.push(ListenAddress::tcp(addr, true));
    }
    if let Some(path) = &listen.unix_path {
        out.push(ListenAddress::unix(path, false));
    }
    if let Some(addr) = listen.tcp_bind {
        out.push(ListenAddress::tcp(addr, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_lines_prefixes_each_new_line_and_tracks_continuation() {
        // The shared stamp-at-newline helper (C's loop, procServ.cc:732-744
        // / clientFactory.cc:264-276). Boundaries: full lines, a partial
        // chunk that leaves the line open, and the continuation that
        // completes it without a fresh stamp.
        let mut in_line = false;
        let a = stamp_lines(b"line1\nline2\n", b"S> ", &mut in_line);
        assert_eq!(a, b"S> line1\nS> line2\n");
        assert!(!in_line, "a trailing newline closes the line");

        // Partial chunk: stamped at the start, no newline → stays open.
        let b = stamp_lines(b"partial", b"S> ", &mut in_line);
        assert_eq!(b, b"S> partial");
        assert!(in_line, "no newline ⟹ line still open");

        // Continuation: NO new stamp until the next newline.
        let c = stamp_lines(b" continued\n", b"S> ", &mut in_line);
        assert_eq!(c, b" continued\n");
        assert!(!in_line);
    }

    #[test]
    fn info_file_matches_manage_procs_format() {
        // C writeInfoFile (procServ.cc:935-941): pid: line then a tcp:/unix:
        // line per listener, in connectionItem::head order (log first).
        let info = InfoSnapshot {
            procserv_pid: 1234,
            addresses: vec![
                ListenAddress::tcp("0.0.0.0:7001".parse().unwrap(), true),
                ListenAddress::tcp("127.0.0.1:7000".parse().unwrap(), false),
            ],
        };
        assert_eq!(
            render_info_file(&info),
            "pid:1234\ntcp:0.0.0.0:7001\ntcp:127.0.0.1:7000\n"
        );
    }

    #[test]
    fn procserv_info_env_uses_pid_ctl_log_form() {
        // C setEnvVar (procServ.cc:943-953): PID=<pid>; then CTL=/LOG= per
        // listener, trailing ';' stripped.
        let info = InfoSnapshot {
            procserv_pid: 1234,
            addresses: vec![
                ListenAddress::tcp("0.0.0.0:7001".parse().unwrap(), true),
                ListenAddress::tcp("127.0.0.1:7000".parse().unwrap(), false),
            ],
        };
        assert_eq!(
            render_procserv_info_env(&info),
            "PID=1234;LOG=tcp:0.0.0.0:7001;CTL=tcp:127.0.0.1:7000"
        );
    }

    #[test]
    fn procserv_info_env_strips_trailing_semicolon_with_no_listeners() {
        let info = InfoSnapshot {
            procserv_pid: 42,
            addresses: vec![],
        };
        assert_eq!(render_procserv_info_env(&info), "PID=42");
        assert_eq!(render_info_file(&info), "pid:42\n");
    }

    #[test]
    fn listen_addresses_orders_log_then_unix_then_control() {
        let listen = ListenConfig {
            tcp_port: Some(7000),
            tcp_bind: Some("127.0.0.1:7000".parse().unwrap()),
            log_port: Some(7001),
            log_bind: Some("0.0.0.0:7001".parse().unwrap()),
            unix_path: Some(PathBuf::from("/run/ioc.sock")),
        };
        let addrs = listen_addresses(&listen);
        let tokens: Vec<(&str, bool)> = addrs
            .iter()
            .map(|a| (a.addr.as_str(), a.readonly))
            .collect();
        assert_eq!(
            tokens,
            vec![
                ("tcp:0.0.0.0:7001", true),
                ("unix:/run/ioc.sock", false),
                ("tcp:127.0.0.1:7000", false),
            ]
        );
    }

    #[tokio::test]
    async fn log_file_prefixes_each_line_with_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        // The bracketing is part of the stamp format string (C's default
        // stampFormat shape), applied raw — not added by the writer.
        let log = LogFile::open(&path, true, "[%Y-%m-%dT%H:%M:%S] ".to_string())
            .await
            .unwrap();

        log.write_chunk(b"line1\nline2\n").await.unwrap();
        log.write_chunk(b"partial").await.unwrap();
        log.write_chunk(b" continued\n").await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            // Every line begins with the `[...]` stamp from the format.
            assert!(line.starts_with('['), "no stamp on: {line}");
        }
        assert!(lines[0].ends_with("line1"));
        assert!(lines[1].ends_with("line2"));
        assert!(lines[2].ends_with("partial continued"));
    }

    #[tokio::test]
    async fn log_stamp_is_applied_raw_without_added_brackets() {
        // An un-bracketed stamp format must be honored verbatim — the
        // writer adds no `[..]` of its own (C applies stampFormat raw,
        // procServ.cc:721; only the *default* stampFormat is bracketed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.log");
        let log = LogFile::open(&path, true, "RAWSTAMP ".to_string())
            .await
            .unwrap();

        log.write_chunk(b"hello\n").await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let line = content.lines().next().unwrap();
        assert!(
            line.starts_with("RAWSTAMP "),
            "stamp format must be applied raw, got: {line}"
        );
        assert!(!line.contains('['), "writer must not add brackets: {line}");
        assert!(line.ends_with("hello"));
    }

    #[tokio::test]
    async fn unstamped_log_is_byte_identical_to_child_output() {
        // C default `stampLog == false` writes the child's bytes verbatim
        // (procServ.cc:82,744). Even with a stamp_format configured, no
        // prefix is added when stamping is off, across multiple chunks and
        // a mid-line partial write.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.log");
        let log = LogFile::open(&path, false, "[%Y] ".to_string())
            .await
            .unwrap();

        log.write_chunk(b"line1\nline2\n").await.unwrap();
        log.write_chunk(b"partial").await.unwrap();
        log.write_chunk(b" continued\n").await.unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, b"line1\nline2\npartial continued\n");
    }

    #[tokio::test]
    async fn reopen_writes_to_a_fresh_file_after_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.log");
        let log = LogFile::open(&path, true, "%Y-%m-%dT%H:%M:%S".to_string())
            .await
            .unwrap();

        log.write_chunk(b"before\n").await.unwrap();

        // Simulate logrotate: move the live file aside. The old handle
        // still points at the renamed inode.
        let rotated = dir.path().join("rot.log.1");
        std::fs::rename(&path, &rotated).unwrap();

        // SIGHUP → reopen: subsequent writes must land in a brand-new
        // file at the original path, not the rotated inode.
        log.reopen().await.unwrap();
        log.write_chunk(b"after\n").await.unwrap();

        let fresh = std::fs::read_to_string(&path).unwrap();
        assert!(
            fresh.contains("after"),
            "new file should hold post-reopen line"
        );
        assert!(
            !fresh.contains("before"),
            "new file must not contain pre-rotation content"
        );

        let old = std::fs::read_to_string(&rotated).unwrap();
        assert!(
            old.contains("before"),
            "rotated file keeps pre-rotation line"
        );
        assert!(
            !old.contains("after"),
            "rotated file must not gain new writes"
        );
    }

    #[test]
    fn pid_file_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pid_file(&path, 12345).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), "12345");
    }
}
