//! Put-event logging.
//!
//! Records client put operations to a log file. Corresponds to
//! C++ ca-gateway's `-putlog` option.
//!
//! ## Logging scope ([`PutLogScope`]) and line shape ([`PutLogLine`])
//!
//! The *scope* — which writes produce a line — is selected by the write
//! hook, not by this module; each scope emits its own line shape, and the
//! two shapes are NOT the same line with an optional field:
//!
//! - [`PutLogScope::TrapWrite`] (default, C contract): record only granted
//!   writes whose matched WRITE rule carries `TRAPWRITE`. C ca-gateway
//!   gates all put-log emission on `asclient->clientPvt()->trapMask`
//!   (`gateVc.cc:236`) and only reaches `gateVcChan::write` for writes
//!   access security already granted, so denied and non-trapped writes are
//!   never logged. The default C build (`#ifndef WITH_CAPUTLOG`) writes
//!   ONLY `"%s %s@%s %s\n"` — timestamp, user@host, PV — with NO value and
//!   NO old (`gateVc.cc:240`):
//!
//!   ```text
//!   Apr 09 14:35:21 user@host TEMP:setpoint
//!   ```
//!
//!   The value/old data-rich format (`gateResources.cc:486`,
//!   `"%s %s@%s %s %s old=%s\n"`) is reachable only in the optional
//!   caPutLog-enabled build (the `#else /* WITH_CAPUTLOG */` branch;
//!   `putLog()` is declared under `#ifdef WITH_CAPUTLOG`,
//!   `gateResources.h:128`), which the checked-in `configure/RELEASE`
//!   leaves disabled — so it is NOT part of the default `--putlog` line.
//!
//! - [`PutLogScope::AllWrites`] (opt-in `--putlog-all`, broader audit):
//!   record *every* client write attempt with the put value, the cached
//!   `old=` value, and its outcome (`OK` / `FAILED` / `DENIED`), including
//!   access-denied and upstream-failed writes — a fail-loud superset that
//!   is not the C contract:
//!
//!   ```text
//!   Apr 09 14:35:22 guest@1.2.3.4 PRESSURE:cmd 100.0 old=? DENIED
//!   ```
//!
//! The line shape is the [`PutLogLine`] variant passed to [`PutLog::log`]:
//! [`PutLogLine::TrapWrite`] writes the C default valueless line,
//! [`PutLogLine::AllWrites`] writes the value/old/outcome audit line.

/// Which client writes the put log records. Selected by the write hook
/// from gateway configuration; see the module docs for the per-mode line
/// format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PutLogScope {
    /// C ca-gateway contract (default): only granted writes to PVs whose
    /// matched WRITE rule carries `TRAPWRITE` are logged, as the default
    /// build's valueless `timestamp user@host pv` line — no value, no old,
    /// no outcome token (`gateVc.cc:236`, `gateVc.cc:240`).
    #[default]
    TrapWrite,
    /// Broader fail-loud audit (opt-in): every client write attempt is
    /// logged with its `OK`/`FAILED`/`DENIED` outcome, including
    /// access-denied and non-trapped writes.
    AllWrites,
}

use std::path::PathBuf;

use chrono::Local;
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

/// One putlog line's content — selects the file line shape.
///
/// The two shapes are not the same line with an optional field: the
/// default C build (`#ifndef WITH_CAPUTLOG`, `gateVc.cc:240`) logs ONLY
/// `timestamp user@host pv`, while the value/`old=` data-rich format
/// belongs to the optional caPutLog-enabled build (`gateResources.cc:486`,
/// declared under `#ifdef WITH_CAPUTLOG` at `gateResources.h:128`).
/// Modeling them as separate variants keeps the value/old payload off the
/// C-default line by construction — the illegal "default line with value"
/// state is unrepresentable.
#[derive(Debug, Clone, Copy)]
pub enum PutLogLine<'a> {
    /// C default trapped-write file line: `"{ts} {user}@{host} {pv}"` —
    /// no value, no old, no outcome token (`gateVc.cc:240`).
    TrapWrite,
    /// Rust-only fail-loud audit line (`--putlog-all`): the put `value`,
    /// the gateway's cached `old=` value, and an `OK`/`FAILED`/`DENIED`
    /// outcome token. Not the C default contract.
    AllWrites {
        value: &'a str,
        old: &'a str,
        outcome: PutOutcome,
    },
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
    /// `line` selects the file line shape ([`PutLogLine`]):
    /// [`PutLogLine::TrapWrite`] writes the C default valueless line
    /// `"{ts} {u}@{h} {pv}"` (`gateVc.cc:240`); [`PutLogLine::AllWrites`]
    /// writes the opt-in audit line `"{ts} {u}@{h} {pv} {val} old={old} {outcome}"`.
    ///
    /// For the audit line, `old` is the gateway's last cached upstream
    /// value before this put — the C caPutLog build logs `vc->eventData()`
    /// in that position (`gateResources.cc:486-492`); callers pass `"?"`
    /// when no monitor value has been cached, matching C's
    /// `old_value == NULL` → `acOldVal = "?"` (`gateResources.cc:476-480`).
    pub async fn log(
        &self,
        user: &str,
        host: &str,
        pv: &str,
        line: PutLogLine<'_>,
    ) -> BridgeResult<()> {
        // Match C ca-gateway's `timeStamp()` (gateResources.cc:73-84):
        // `localtime()` + `strftime("%b %d %H:%M:%S")` → e.g.
        // `Apr 09 14:35:21` — local time, abbreviated month + day +
        // HH:MM:SS, NO year, NO subseconds. Use `Local` (not `Utc`) to
        // mirror `localtime()`.
        let timestamp = Local::now().format("%b %d %H:%M:%S").to_string();
        let line = match line {
            // C default build (`#ifndef WITH_CAPUTLOG`, gateVc.cc:240):
            // timestamp, user@host, PV — no value, no old, no token.
            PutLogLine::TrapWrite => format!("{timestamp} {user}@{host} {pv}\n"),
            // Opt-in fail-loud audit line: value, cached old=, outcome.
            PutLogLine::AllWrites {
                value,
                old,
                outcome,
            } => format!(
                "{} {}@{} {} {} old={} {}\n",
                timestamp,
                user,
                host,
                pv,
                value,
                old,
                outcome.as_str()
            ),
        };

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
        // AllWrites lines carry value, old=, and the outcome token.
        log.log(
            "alice",
            "host1",
            "TEMP",
            PutLogLine::AllWrites {
                value: "25.0",
                old: "24.8",
                outcome: PutOutcome::Ok,
            },
        )
        .await
        .unwrap();
        log.log(
            "bob",
            "host2",
            "PRESS",
            PutLogLine::AllWrites {
                value: "100",
                old: "?",
                outcome: PutOutcome::Denied,
            },
        )
        .await
        .unwrap();
        log.log(
            "eve",
            "host3",
            "VAC",
            PutLogLine::AllWrites {
                value: "1e-6",
                old: "2e-6",
                outcome: PutOutcome::Failed,
            },
        )
        .await
        .unwrap();
        // TrapWrite line: C default valueless line.
        log.log("max", "host4", "FLOW", PutLogLine::TrapWrite)
            .await
            .unwrap();

        // Read back
        let content = std::fs::read_to_string(&temp).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("alice@host1 TEMP 25.0 old=24.8 OK"));
        assert!(lines[1].contains("bob@host2 PRESS 100 old=? DENIED"));
        assert!(lines[2].contains("eve@host3 VAC 1e-6 old=2e-6 FAILED"));
        // The C default TrapWrite line ends at the PV name — no value,
        // no old=, no outcome token.
        assert!(lines[3].ends_with("max@host4 FLOW"));
        assert!(
            !lines[3].contains("old=") && !lines[3].contains("OK"),
            "TrapWrite line must carry no value/old and no outcome token"
        );

        let _ = std::fs::remove_file(&temp);
    }

    /// Regression R0604-BRCAGW-PUTLOG-BODY-WITHOUT-CAPUTLOG-1.
    ///
    /// The default `--putlog` ([`PutLogScope::TrapWrite`]) file line must
    /// be exactly the C default build's `"%s %s@%s %s\n"` —
    /// `timestamp user@host pv` — with NO value and NO `old=` field. Those
    /// fields belong to the optional caPutLog-enabled build
    /// (`#else /* WITH_CAPUTLOG */`, `gateResources.cc:486`), which the
    /// checked-in `configure/RELEASE` leaves disabled, so they are not part
    /// of the default putlog line.
    #[tokio::test]
    async fn trapwrite_line_is_c_default_no_value_or_old() {
        let temp =
            std::env::temp_dir().join(format!("ca_gateway_putlog_trap_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&temp);

        let log = PutLog::new(temp.clone());
        log.log("op", "ws3", "TEMP:setpoint", PutLogLine::TrapWrite)
            .await
            .unwrap();

        let content = std::fs::read_to_string(&temp).unwrap();
        let line = content.lines().next().expect("one log line");
        // The body after the three timestamp tokens is exactly
        // "user@host pv" — split off the timestamp and compare.
        let toks: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            &toks[3..],
            &["op@ws3", "TEMP:setpoint"],
            "C default line body is `user@host pv` only, got {line:?}"
        );
        assert!(
            !line.contains("old="),
            "default putlog line must not carry an `old=` field, got {line:?}"
        );

        let _ = std::fs::remove_file(&temp);
    }

    /// The leading timestamp matches C ca-gateway's `timeStamp()`
    /// (`gateResources.cc:81` `strftime("%b %d %H:%M:%S")`): abbreviated
    /// month + day + HH:MM:SS, local time, NO year, NO subseconds — not
    /// the pre-fix ISO-8601 `Utc::now().to_rfc3339()`.
    #[tokio::test]
    async fn timestamp_matches_c_strftime_format() {
        let temp =
            std::env::temp_dir().join(format!("ca_gateway_putlog_ts_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&temp);

        let log = PutLog::new(temp.clone());
        log.log("alice", "host1", "TEMP", PutLogLine::TrapWrite)
            .await
            .unwrap();

        let content = std::fs::read_to_string(&temp).unwrap();
        let line = content.lines().next().expect("one log line");
        // C `timeStamp()` is three space-separated tokens before the body:
        // "Mon DD HH:MM:SS user@host ...".
        let toks: Vec<&str> = line.split(' ').collect();
        // tok[0] = abbreviated month (3 alpha chars), parseable by chrono.
        assert_eq!(toks[0].len(), 3, "month abbrev, got {:?}", toks[0]);
        assert!(
            toks[0].chars().all(|c| c.is_ascii_alphabetic()),
            "month abbrev must be alphabetic, got {:?}",
            toks[0]
        );
        // tok[2] = HH:MM:SS (two colons, eight chars).
        assert_eq!(toks[2].len(), 8, "HH:MM:SS, got {:?}", toks[2]);
        assert_eq!(
            toks[2].matches(':').count(),
            2,
            "HH:MM:SS has two colons, got {:?}",
            toks[2]
        );
        // The body follows the timestamp's three tokens.
        assert_eq!(toks[3], "alice@host1");
        // NOT RFC3339: no 'T' date-time separator, no '+00:00' offset.
        let ts = format!("{} {} {}", toks[0], toks[1], toks[2]);
        assert!(!ts.contains('T'), "must not be ISO-8601, got {ts:?}");
        assert!(!ts.contains('+'), "must not carry a tz offset, got {ts:?}");

        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn outcome_as_str() {
        assert_eq!(PutOutcome::Ok.as_str(), "OK");
        assert_eq!(PutOutcome::Denied.as_str(), "DENIED");
        assert_eq!(PutOutcome::Failed.as_str(), "FAILED");
    }
}
