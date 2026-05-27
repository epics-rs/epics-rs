//! Put-event logging.
//!
//! Records all client put operations to a log file. Corresponds to
//! C++ ca-gateway's `-putlog` option.
//!
//! Each put generates one line in the log. The `old=` field carries the
//! gateway's last cached upstream value before the put (C ca-gateway logs
//! `vc->eventData()` here; `old=?` when no monitor value has been cached
//! yet):
//!
//! ```text
//! 2026-04-09T14:35:21.123Z user@host TEMP:setpoint 25.0 old=24.8 OK
//! 2026-04-09T14:35:22.456Z guest@1.2.3.4 PRESSURE:cmd 100.0 old=? DENIED
//! ```
//!
//! ## Deviation from C ca-gateway (intentional, broader audit)
//!
//! C logs a put only for PVs whose access-security client has the
//! `TRAPWRITE` / `trapMask` flag set (`gateVc.cc:236`), and only the
//! write *attempt* — a put rejected by access security before
//! `gateVcChan::write` is reached is never logged. pva-rs has no per-PV
//! `trapMask` concept: it logs **every** client put and records the
//! outcome (`OK` / `FAILED` / `DENIED`), including access-denied and
//! upstream-failed writes. This is a deliberately broader, fail-loud
//! audit trail — every write attempt against the gateway is recorded,
//! not only those an operator pre-flagged with `TRAPWRITE`.

use std::path::PathBuf;

use chrono::Utc;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::error::BridgeResult;

/// Default rotation threshold: 100 MiB. Rolls `path` to `path.1`
/// (overwriting any prior `.1`) and re-opens. Operators typically
/// run logrotate on top; this is the in-process safety net so the
/// gateway doesn't fill the partition between cron ticks.
const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Outcome of a put attempt for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// Put accepted and forwarded upstream.
    Ok,
    /// Put rejected (read-only mode, ACL deny, etc.).
    Denied,
    /// Put forwarded but upstream returned an error.
    Failed,
}

impl PutOutcome {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Denied => "DENIED",
            Self::Failed => "FAILED",
        }
    }
}

/// Put-event logger.
///
/// Writes to a file with line-buffered async I/O. Multiple concurrent
/// writers are serialized via an internal mutex. When the file grows
/// past `max_bytes`, it is renamed to `<path>.1` (overwriting any
/// existing `.1`) and a fresh file is opened. Operators are still
/// expected to run logrotate; this is the in-process backstop so a
/// chatty gateway can't fill its disk between rotation ticks.
pub struct PutLog {
    path: PathBuf,
    /// Mutex around the file handle so concurrent writers are serialized.
    file: Mutex<Option<tokio::fs::File>>,
    /// Approximate byte count of the current file (tracked since open
    /// so we don't `metadata()` on every write). Reset to 0 after
    /// rotation.
    bytes_written: Mutex<u64>,
    max_bytes: u64,
}

impl PutLog {
    /// Create a new logger writing to `path`. Opens (or creates) the file
    /// in append mode lazily on the first write. Default rotation
    /// threshold is 100 MiB; override with [`Self::with_max_bytes`].
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
            bytes_written: Mutex::new(0),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Override the rotation threshold (bytes).
    pub fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// Log a put event.
    ///
    /// `old` is the gateway's last cached upstream value before this put,
    /// rendered into the `old=` field — the C ca-gateway logs
    /// `vc->eventData()` (the virtual connection's cached monitor value)
    /// in the same position (gateResources.cc:486-492). Callers pass
    /// `"?"` when no monitor value has been cached, matching C's
    /// `old_value == NULL` → `acOldVal = "?"` (gateResources.cc:476-480).
    pub async fn log(
        &self,
        user: &str,
        host: &str,
        pv: &str,
        value: &str,
        old: &str,
        outcome: PutOutcome,
    ) -> BridgeResult<()> {
        let timestamp = Utc::now().to_rfc3339();
        let line = format!(
            "{} {}@{} {} {} old={} {}\n",
            timestamp,
            user,
            host,
            pv,
            value,
            old,
            outcome.as_str()
        );

        let mut guard = self.file.lock().await;
        if guard.is_none() {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await?;
            // Initialise byte counter from existing file size so a
            // restart picks up the rotation threshold mid-cycle.
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            *self.bytes_written.lock().await = len;
            *guard = Some(f);
        }

        if let Some(f) = guard.as_mut() {
            f.write_all(line.as_bytes()).await?;
            f.flush().await?;
        }
        let mut counter = self.bytes_written.lock().await;
        *counter = counter.saturating_add(line.len() as u64);

        if *counter >= self.max_bytes {
            // NEW-3: hold `guard` across the rename so a concurrent
            // `log()` call cannot acquire `self.file.lock()` between
            // the take-and-drop and the rename. The previous order
            // (drop guard, then rename) admitted a race where the
            // racing log() reopened `<path>` (creating a fresh file)
            // before our rename moved that fresh file aside, putting
            // the audit lines into `<path>.1` while `<path>` stayed
            // empty.
            *counter = 0;
            *guard = None;
            let backup = self.path.with_extension(
                self.path
                    .extension()
                    .map(|e| format!("{}.1", e.to_string_lossy()))
                    .unwrap_or_else(|| "1".to_string()),
            );
            if let Err(e) = tokio::fs::rename(&self.path, &backup).await {
                tracing::warn!(
                    error = %e,
                    src = %self.path.display(),
                    dst = %backup.display(),
                    "putlog rotation rename failed; continuing without rotation"
                );
            }
            // Guards drop here at end of scope, after the rename.
            drop(guard);
            drop(counter);
        }

        Ok(())
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_to_temp_file() {
        let temp =
            std::env::temp_dir().join(format!("ca_gateway_putlog_test_{}.log", std::process::id()));
        // Cleanup any leftover from previous test runs
        let _ = std::fs::remove_file(&temp);

        let log = PutLog::new(temp.clone());
        log.log("alice", "host1", "TEMP", "25.0", "24.8", PutOutcome::Ok)
            .await
            .unwrap();
        log.log("bob", "host2", "PRESS", "100", "?", PutOutcome::Denied)
            .await
            .unwrap();
        log.log("eve", "host3", "VAC", "1e-6", "2e-6", PutOutcome::Failed)
            .await
            .unwrap();

        // Read back
        let content = std::fs::read_to_string(&temp).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("alice@host1 TEMP 25.0 old=24.8 OK"));
        assert!(lines[1].contains("bob@host2 PRESS 100 old=? DENIED"));
        assert!(lines[2].contains("eve@host3 VAC 1e-6 old=2e-6 FAILED"));

        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn outcome_as_str() {
        assert_eq!(PutOutcome::Ok.as_str(), "OK");
        assert_eq!(PutOutcome::Denied.as_str(), "DENIED");
        assert_eq!(PutOutcome::Failed.as_str(), "FAILED");
    }
}
