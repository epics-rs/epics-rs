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

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::path::Path;
use std::sync::{Arc, Mutex};

/// Where audit events go. The bundled implementations cover the two
/// common cases (file with append-write, stderr) but a custom `Sink`
/// can wrap an HTTP shipper, syslog, or similar.
pub enum AuditSink {
    File(AuditFile),
    Stderr,
    Custom(Box<dyn AuditWriter + Send + Sync>),
}

/// The append-only file behind [`AuditSink::File`].
///
/// Opaque on purpose. It used to be a bare `Mutex<tokio::fs::File>` in the
/// variant, and that spelling does not build a working audit log under
/// `rtems-exec-model`: `tokio::fs` is a blocking `std::fs` call handed to
/// tokio's `spawn_blocking` pool, so it requires an entered tokio runtime,
/// and the writer task this sink is drained by runs on the background
/// executor there, not on a tokio worker. The write now goes through
/// [`epics_base_rs::runtime::fs`], which both backends implement.
///
/// Holding the handle behind this type instead of exposing the mutex means a
/// caller cannot put a runtime-bound handle back into the variant.
pub struct AuditFile(Arc<Mutex<std::fs::File>>);

impl AuditFile {
    /// Adopt an already-open file. Synchronous, so a caller that opened the
    /// file itself (`CaServer`'s `EPICS_CAS_AUDIT_FILE` handling) needs no
    /// runtime to build the sink.
    pub fn from_std(file: std::fs::File) -> Self {
        Self(Arc::new(Mutex::new(file)))
    }
}

impl std::fmt::Debug for AuditFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuditFile")
    }
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
        let path = path.as_ref().to_path_buf();
        let f = epics_base_rs::runtime::fs::blocking(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        })
        .await?;
        Ok(AuditSink::File(AuditFile::from_std(f)))
    }

    pub async fn write(&self, line: &str) {
        // THE write point for every sink and every renderer. One record in,
        // one line out — `to_aslog_line` interpolates the peer-supplied
        // `user`, `host`, `pv` and `value` straight into a text line, and a
        // CA CLIENT_NAME may legally contain a newline (the server checks
        // only NUL-termination and a 511-byte cap, `server/tcp.rs:2416`).
        // Applied uniformly rather than per-renderer: the JSON encoder has
        // already removed every raw control byte, so this is a no-op there.
        let line = &*epics_base_rs::runtime::log::single_line(line);
        match self {
            AuditSink::File(AuditFile(handle)) => {
                // The lock is taken inside the closure rather than held across
                // an await, so the "never interleave mid-line" invariant is
                // unchanged: two concurrent events still serialise on this one
                // mutex, now on the blocking worker instead of on the task.
                let mut bytes = line.as_bytes().to_vec();
                bytes.push(b'\n');
                let handle = handle.clone();
                let _ = epics_base_rs::runtime::fs::blocking(move || {
                    use std::io::Write as _;
                    let mut f = handle.lock().unwrap_or_else(|e| e.into_inner());
                    f.write_all(&bytes)?;
                    f.flush()
                })
                .await;
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
        // `runtime::task::spawn`, not `tokio::spawn`: this is the one task that
        // ever touches the sink, so if it cannot be created on a backend then
        // nothing the sink does matters. `tokio::sync::mpsc` stays — it
        // suspends on a runtime-agnostic primitive and needs no reactor.
        epics_base_rs::runtime::task::spawn(async move {
            while let Some(line) = rx.recv().await {
                sink.write(&line).await;
            }
        });
        Self { tx, format }
    }

    pub fn log(&self, ev: AuditEvent<'_>) {
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

    /// The point of the change: an audit file can be opened and written with
    /// no tokio runtime anywhere in the picture — a plain `std::thread`
    /// driving the future with `park_on`, which is the shape
    /// `BlockingCaServer` uses for every per-client command.
    ///
    /// Feature-gated because it is only true on the exec backend. On the
    /// tokio backend `runtime::task::spawn_blocking` is `tokio::task::
    /// spawn_blocking`, which needs an entered runtime exactly as `tokio::fs`
    /// did — the seam does not invent a blocking pool where there is none, it
    /// routes to whichever one the backend has.
    #[cfg(feature = "rtems-exec-model")]
    #[test]
    fn a_file_sink_writes_with_no_runtime_entered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let p = path.clone();
        std::thread::spawn(move || {
            epics_base_rs::runtime::task::block_on_sync(async move {
                let sink = AuditSink::file(&p).await.expect("open through the seam");
                sink.write("one").await;
                sink.write("two").await;
            })
            .expect("a bare thread has no runtime, so park_on drives it");
        })
        .join()
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            body, "one\ntwo\n",
            "both lines landed, each terminated, in order"
        );
    }

    /// Neither this module nor `replay` may name the crate the seam replaces.
    ///
    /// Comment lines are stripped before the search, not because they do not
    /// matter but because both files must be free to *explain* the spelling
    /// they no longer use — and a whole-file search matches the explanation.
    /// Measured: the first version of this test failed on `EventRecorder`'s
    /// own doc comment saying what the handle used to be.
    #[test]
    fn the_audit_and_replay_writers_do_not_name_tokio_fs() {
        for (name, src) in [
            ("audit.rs", include_str!("audit.rs")),
            ("replay.rs", include_str!("replay.rs")),
        ] {
            let code: String = src
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains(concat!("tokio", "::fs")),
                "{name} reached for the convenient spelling again"
            );
        }
    }

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

    /// B1, second instance: `to_aslog_line` interpolates every peer-supplied
    /// field into a text line with no escaping, and a CA `CLIENT_NAME` may
    /// legally contain a newline — the server checks only NUL-termination
    /// and a 511-byte cap (`server/tcp.rs:2416`). A VERIFIED cap-token's
    /// `claims.sub` becomes `state.username` (`tcp.rs:2474`) and lands in
    /// the same field, so this needs neither a forged identity nor a
    /// rejected token.
    ///
    /// Boundary sweep over the fields, and over both renderers, because the
    /// framing is applied at the sink and must therefore hold for whichever
    /// one is configured.
    #[tokio::test]
    async fn no_wire_field_can_forge_a_second_audit_record() {
        const FORGE: &str = "x\n04/09/2026 09:00:00 ASUSER W root@localhost caput: SAFETY:ILK=0 ok";

        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait::async_trait]
        impl AuditWriter for Capture {
            async fn write_line(&self, line: &str) {
                self.0.lock().unwrap().push(line.to_string());
            }
        }

        for field in ["user", "host", "pv", "value", "result"] {
            let pick = |name: &str, clean: &'static str| {
                if name == field { FORGE } else { clean }
            };
            let ev = AuditEvent {
                event: "caput",
                peer: "10.0.0.5:1234",
                user: pick("user", "alice"),
                host: pick("host", "opi-1"),
                pv: pick("pv", "MOTOR:VAL"),
                value: pick("value", "3.14"),
                result: pick("result", "ok"),
            };
            for (name, rendered) in [("aslog", ev.to_aslog_line()), ("json", ev.to_json())] {
                let cap = Capture::default();
                let sink = AuditSink::Custom(Box::new(cap.clone()));
                sink.write(&rendered).await;
                let got = cap.0.lock().unwrap().clone();
                assert_eq!(got.len(), 1, "{name}/{field}: one event, one write");
                assert!(
                    !got[0].contains('\n') && !got[0].contains('\r'),
                    "{name}/{field}: the record must not carry a line break: {:?}",
                    got[0]
                );
                assert!(
                    got[0].contains("SAFETY:ILK"),
                    "{name}/{field}: the injected text must survive, escaped"
                );
            }
        }

        // The JSON renderer's own escaping must survive the sink unchanged —
        // this is what makes one uniform rule at the writer safe.
        let ev = AuditEvent {
            event: "caput",
            peer: "p",
            user: "a\nb",
            host: "h",
            pv: "P",
            value: "v",
            result: "ok",
        };
        assert!(
            ev.to_json().contains(r#""user":"a\nb""#),
            "the JSON encoder escapes the newline itself"
        );
        let json = ev.to_json();
        assert_eq!(
            &*epics_base_rs::runtime::log::single_line(&json),
            json,
            "the sink must not re-escape already-escaped JSON"
        );
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
