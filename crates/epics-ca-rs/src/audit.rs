//! Structured audit log for security-relevant CA server events.
//!
//! Goes beyond the regular `tracing` instrumentation: this is a single
//! append-only stream meant for compliance / forensic review. Every
//! event lands as one JSON line with a stable schema, and the writer
//! is held behind a mutex so two concurrent events never interleave
//! mid-line.
//!
//! Wire it in by passing `AuditSink` into `CaServerBuilder::audit()`.
//! Without configuration the server emits no audit log; the runtime
//! cost is one `Option::is_some()` check per event.
//!
//! Schema (kept terse so log files stay manageable):
//!
//! ```json
//! {"ts":"2026-04-27T10:15:30.123Z","ev":"caput","peer":"10.0.0.5:54311",
//!  "user":"alice","host":"opi-1","pv":"MOTOR:VAL","value":"3.14",
//!  "result":"ok"}
//! ```
//!
//! Event types: `connect`, `disconnect`, `create_chan`, `caget`,
//! `caput`, `acf_deny`, `subscribe`, `unsubscribe`. Keep additions
//! strictly additive — downstream log shippers parse the JSON.

use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Where audit events go. The bundled implementations cover the two
/// common cases (file with append-write, stderr) but a custom `Sink`
/// can wrap an HTTP shipper, syslog, or similar.
pub enum AuditSink {
    File(Mutex<tokio::fs::File>),
    Stderr,
    Custom(Box<dyn AuditWriter + Send + Sync>),
}

/// Hook for application-supplied audit destinations.
#[async_trait::async_trait]
pub trait AuditWriter {
    async fn write_line(&self, line: &str);
}

impl AuditSink {
    /// Open a file in append mode. Each call appends; the file is
    /// neither truncated nor rotated — pair with `logrotate` or
    /// systemd-journald via stderr.
    pub async fn file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(AuditSink::File(Mutex::new(f)))
    }

    pub async fn write(&self, line: &str) {
        match self {
            AuditSink::File(m) => {
                let mut f = m.lock().await;
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
                let _ = f.flush().await;
            }
            AuditSink::Stderr => {
                eprintln!("{line}");
            }
            AuditSink::Custom(w) => {
                w.write_line(line).await;
            }
        }
    }
}

/// One audit event. Fields are intentionally flat for grep-ability;
/// values are escape-quoted JSON strings.
#[derive(Debug, Clone)]
pub struct AuditEvent<'a> {
    pub event: &'a str,
    pub peer: &'a str,
    pub user: &'a str,
    pub host: &'a str,
    /// PV / channel name. Empty for connect/disconnect.
    pub pv: &'a str,
    /// String rendering of the new value for `caput`. Empty otherwise.
    pub value: &'a str,
    /// "ok", "denied", "fail", or empty.
    pub result: &'a str,
}

/// Output format for [`AuditLogger`]. JSON is the modern default
/// (one event per line, easily ingested by Splunk / Loki / ELK).
/// `LegacyAslog` mirrors the libca `asLib` text format that pre-Rust
/// EPICS sites already have parsing tooling for:
///
/// ```text
/// 04/29/2026 14:35:21 ASUSER W alice@opi-1 write: MOTOR:VAL=3.14 ok
/// ```
///
/// Pick this when an existing audit pipeline already consumes the
/// libca format and the rust IOC needs to feed into it without
/// touching the downstream parsers.
#[derive(Clone, Copy, Debug, Default)]
pub enum AuditFormat {
    /// Modern one-line JSON (default).
    #[default]
    Json,
    /// libca asLib-compatible single-line text format.
    LegacyAslog,
}

impl AuditEvent<'_> {
    /// libca-asLib-compatible single-line text rendering. Used by
    /// [`AuditFormat::LegacyAslog`]. Format:
    ///
    /// `MM/DD/YYYY HH:MM:SS ASUSER <op> <user>@<host> <verb>: <pv>[=<value>] <result>`
    ///
    /// `<op>` is `R` (read) for `subscribe` / `unsubscribe` / `caget`,
    /// `W` (write) for `caput`, `C` (connect) / `D` (disconnect) for
    /// connection lifecycle, `O` (open) for `create_chan`, `X` for
    /// `acf_deny`, or `?` for any other event type. `<verb>` is the
    /// full event name so downstream parsers can disambiguate.
    fn to_aslog_line(&self) -> String {
        let now = chrono::Utc::now();
        let ts = now.format("%m/%d/%Y %H:%M:%S");
        let op = match self.event {
            "subscribe" | "unsubscribe" | "caget" => "R",
            "caput" => "W",
            "connect" => "C",
            "disconnect" => "D",
            "create_chan" => "O",
            "acf_deny" => "X",
            _ => "?",
        };
        let identity = if self.user.is_empty() && self.host.is_empty() {
            self.peer.to_string()
        } else if self.host.is_empty() {
            self.user.to_string()
        } else if self.user.is_empty() {
            format!("anonymous@{}", self.host)
        } else {
            format!("{}@{}", self.user, self.host)
        };
        let pv_value = if self.value.is_empty() {
            self.pv.to_string()
        } else {
            format!("{}={}", self.pv, self.value)
        };
        let result = if self.result.is_empty() {
            String::new()
        } else {
            format!(" {}", self.result)
        };
        let line = format!(
            "{ts} ASUSER {op} {identity} {ev}: {pv_value}{result}",
            ev = self.event,
        );
        // Drop the trailing space the format leaves when both
        // pv_value and result are empty (e.g. anonymous connect with
        // no PV) so log shippers don't index hidden whitespace.
        line.trim_end().to_string()
    }

    fn to_json(&self) -> String {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut s = String::with_capacity(192);
        s.push('{');
        push_kv(&mut s, "ts", &ts);
        s.push(',');
        push_kv(&mut s, "ev", self.event);
        s.push(',');
        push_kv(&mut s, "peer", self.peer);
        if !self.user.is_empty() {
            s.push(',');
            push_kv(&mut s, "user", self.user);
        }
        if !self.host.is_empty() {
            s.push(',');
            push_kv(&mut s, "host", self.host);
        }
        if !self.pv.is_empty() {
            s.push(',');
            push_kv(&mut s, "pv", self.pv);
        }
        if !self.value.is_empty() {
            s.push(',');
            push_kv(&mut s, "value", self.value);
        }
        if !self.result.is_empty() {
            s.push(',');
            push_kv(&mut s, "result", self.result);
        }
        s.push('}');
        s
    }
}

fn push_kv(s: &mut String, k: &str, v: &str) {
    s.push('"');
    s.push_str(k);
    s.push_str("\":\"");
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

/// Convenience handle. The server wraps this in an Arc and clones it
/// to per-connection tasks. Internally a bounded mpsc decouples the
/// hot caller path from the sink: a slow disk drops audit lines
/// (counted in `ca_server_audit_drops_total`) instead of blocking the
/// CA connection. The `Option` at the call sites lets the hot path
/// skip work when no logger is configured.
#[derive(Clone)]
pub struct AuditLogger {
    tx: tokio::sync::mpsc::Sender<String>,
    format: AuditFormat,
}

const AUDIT_QUEUE_CAPACITY: usize = 4096;

impl AuditLogger {
    /// Wrap a sink and spawn a single writer task. The writer drains
    /// the queue and serializes writes; if the queue fills, new
    /// events are dropped at `log()` time so the CA hot path never
    /// stalls on disk I/O. Defaults to [`AuditFormat::Json`].
    pub fn new(sink: AuditSink) -> Self {
        Self::new_with_format(sink, AuditFormat::Json)
    }

    /// Like [`Self::new`] but emits in the chosen format —
    /// pass [`AuditFormat::LegacyAslog`] to feed an existing
    /// libca-asLib-compatible audit pipeline.
    pub fn new_with_format(sink: AuditSink, format: AuditFormat) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(AUDIT_QUEUE_CAPACITY);
        let sink = Arc::new(sink);
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                sink.write(&line).await;
            }
        });
        Self { tx, format }
    }

    pub async fn log(&self, ev: AuditEvent<'_>) {
        let line = match self.format {
            AuditFormat::Json => ev.to_json(),
            AuditFormat::LegacyAslog => ev.to_aslog_line(),
        };
        // try_send: never block the caller. Drop on full queue and
        // count it — losing a line under sustained overload is
        // strictly better than pinning a CA connection.
        if self.tx.try_send(line).is_err() {
            metrics::counter!("ca_server_audit_drops_total").increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_basic() {
        let ev = AuditEvent {
            event: "caput",
            peer: "10.0.0.5:1234",
            user: "alice",
            host: "opi-1",
            pv: "MOTOR:VAL",
            value: "3.14",
            result: "ok",
        };
        let s = ev.to_json();
        assert!(s.contains("\"ev\":\"caput\""));
        assert!(s.contains("\"pv\":\"MOTOR:VAL\""));
        assert!(s.contains("\"result\":\"ok\""));
    }

    /// libca asLib-compatible text format. Verifies the line
    /// shape, op-letter mapping, identity composition, and
    /// pv=value rendering for `caput`.
    #[test]
    fn aslog_caput_render() {
        let ev = AuditEvent {
            event: "caput",
            peer: "10.0.0.5:1234",
            user: "alice",
            host: "opi-1",
            pv: "MOTOR:VAL",
            value: "3.14",
            result: "ok",
        };
        let s = ev.to_aslog_line();
        // Date/time prefix is stable shape, content varies.
        assert!(s.starts_with(&chrono::Utc::now().format("%m/%d/%Y").to_string()));
        assert!(s.contains(" ASUSER W alice@opi-1 caput: MOTOR:VAL=3.14 ok"));
    }

    /// Read events map to `R` and omit value (no `=`).
    #[test]
    fn aslog_subscribe_render() {
        let ev = AuditEvent {
            event: "subscribe",
            peer: "p",
            user: "bob",
            host: "ws-2",
            pv: "BL10C:VG-01:PRESSURE",
            value: "",
            result: "",
        };
        let s = ev.to_aslog_line();
        assert!(s.contains(" ASUSER R bob@ws-2 subscribe: BL10C:VG-01:PRESSURE"));
        assert!(!s.contains("="));
    }

    /// Empty user/host falls back to peer; no trailing result space.
    #[test]
    fn aslog_anonymous_no_result() {
        let ev = AuditEvent {
            event: "connect",
            peer: "192.0.2.4:55001",
            user: "",
            host: "",
            pv: "",
            value: "",
            result: "",
        };
        let s = ev.to_aslog_line();
        assert!(s.contains(" ASUSER C 192.0.2.4:55001 connect:"));
        assert!(!s.ends_with(' '));
    }

    #[test]
    fn json_escapes_quotes_and_control() {
        let ev = AuditEvent {
            event: "caput",
            peer: "p",
            user: "u",
            host: "h",
            pv: "PV",
            value: "a\"b\nc",
            result: "ok",
        };
        let s = ev.to_json();
        assert!(s.contains("\"value\":\"a\\\"b\\nc\""));
    }

    #[test]
    fn skips_empty_optional_fields() {
        let ev = AuditEvent {
            event: "connect",
            peer: "10.0.0.5:1234",
            user: "",
            host: "",
            pv: "",
            value: "",
            result: "",
        };
        let s = ev.to_json();
        assert!(!s.contains("\"user\""));
        assert!(!s.contains("\"pv\""));
    }
}
