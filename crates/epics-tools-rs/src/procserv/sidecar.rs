//! Side-car file management — log file, info file, pid file, env vars.
//!
//! Mirrors C `openLogFile()`, `writeInfoFile()`, `writePidFile()`,
//! and `setEnvVar()` from `procServ.cc`. These exist to support the
//! `procServUtils/manage-procs` tooling, which inspects pid + info
//! files in a known directory to enumerate / attach / restart
//! procserv instances.
//!
//! Convention preserved exactly (per kodex commit-history note
//! `4d2aee67`): `PROCSERV_INFO` env-var carries `KEY=value` pairs
//! mirroring the info file content.

use std::path::{Path, PathBuf};

use chrono::Local;
use parking_lot::Mutex as SyncMutex;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;

use crate::procserv::error::{ProcServError, ProcServResult};

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
    /// Per-line stamp format, applied RAW (C `stampFormat`,
    /// `procServ.cc:721`). Any bracketing/separator is part of this
    /// string, not added here — C's default `"[" + timeFormat + "] "` is
    /// just the default value, overridable verbatim.
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
    pub async fn open(path: &Path, stamp_format: impl Into<String>) -> ProcServResult<Self> {
        let file = Self::open_handle(path).await?;
        Ok(Self {
            file: AsyncMutex::new(file),
            path: path.to_path_buf(),
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
        // Build the output buffer inside an inner block so the
        // parking_lot guard is unambiguously dropped before the
        // first `.await`. parking_lot's `MutexGuard` is `!Send`, so
        // a guard that lingers in scope across an await poisons the
        // outer future's `Send` bound — the supervisor's `tokio::spawn`
        // would refuse to schedule it.
        let out: Vec<u8> = {
            let stamp = self.format_stamp();
            let mut buf: Vec<u8> = Vec::with_capacity(chunk.len() + 32);
            let mut in_line = self.in_line.lock();
            let mut prev = 0usize;
            for (i, &b) in chunk.iter().enumerate() {
                if !*in_line {
                    buf.extend_from_slice(stamp.as_bytes());
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

/// Status info file. Format matches C procServ + `manage-procs`:
///
/// ```text
/// procservpid=NNNN
/// childpid=NNNN
/// childexe=/path/to/foo
/// childargs=arg1 arg2
/// ```
///
/// Updated whenever the child respawns. Atomic via tmp+rename.
pub fn write_info_file(path: &Path, info: &InfoSnapshot) -> ProcServResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("procserv.info")
    ));
    let body = render_procserv_info(info);
    std::fs::write(&tmp, body).map_err(ProcServError::Io)?;
    std::fs::rename(&tmp, path).map_err(ProcServError::Io)?;
    Ok(())
}

/// Snapshot of supervisor + child state, serialized into the info
/// file and the `PROCSERV_INFO` env var. Construct a fresh one on
/// each child respawn.
#[derive(Debug, Clone)]
pub struct InfoSnapshot {
    pub procserv_pid: i32,
    pub child_pid: Option<i32>,
    pub child_exe: PathBuf,
    pub child_args: Vec<String>,
}

/// Build the `KEY=value` form for `PROCSERV_INFO` env var (passed to
/// the child on `execvp` so the IOC can introspect its supervision
/// context).
pub fn render_procserv_info(info: &InfoSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("procservpid={}\n", info.procserv_pid));
    if let Some(p) = info.child_pid {
        out.push_str(&format!("childpid={p}\n"));
    }
    out.push_str(&format!("childexe={}\n", info.child_exe.display()));
    out.push_str(&format!("childargs={}\n", info.child_args.join(" ")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_info_keys_match_c_procserv_convention() {
        let info = InfoSnapshot {
            procserv_pid: 1234,
            child_pid: Some(1235),
            child_exe: PathBuf::from("/usr/bin/softIoc"),
            child_args: vec!["-d".into(), "test.db".into()],
        };
        let rendered = render_procserv_info(&info);
        assert!(rendered.contains("procservpid=1234"));
        assert!(rendered.contains("childpid=1235"));
        assert!(rendered.contains("childexe=/usr/bin/softIoc"));
        assert!(rendered.contains("childargs=-d test.db"));
    }

    #[tokio::test]
    async fn log_file_prefixes_each_line_with_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        // The bracketing is part of the stamp format string (C's default
        // stampFormat shape), applied raw — not added by the writer.
        let log = LogFile::open(&path, "[%Y-%m-%dT%H:%M:%S] ".to_string())
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
        let log = LogFile::open(&path, "RAWSTAMP ".to_string()).await.unwrap();

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
    async fn reopen_writes_to_a_fresh_file_after_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.log");
        let log = LogFile::open(&path, "%Y-%m-%dT%H:%M:%S".to_string())
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
