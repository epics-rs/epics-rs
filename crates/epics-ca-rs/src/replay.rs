//! On-disk recording and replay of CA observability events.
//!
//! When something goes wrong in production — beacons stop arriving,
//! a record of clients flaps, an IOC's connect/disconnect rate
//! suddenly spikes — what helps most is *watching it happen again*.
//! `tracing` lines and Prometheus samples answer aggregate questions
//! ("how many disconnects this hour?") but lose the per-event timing
//! that explains the *cause*. This module fills the gap by capturing
//! every event into a JSON-Lines file at the moment it occurs and
//! providing a replay tool that streams them back into any consumer.
//!
//! Schema is deliberately small and additive — three event flavours
//! cover the majority of forensic questions:
//!
//! - `beacon_recv`     — a beacon was received from a CA server
//! - `client_connect`  — a TCP client connected to the server
//! - `client_disconnect` — that client closed (graceful or otherwise)
//!
//! Adding fields is forward-compatible; readers ignore unknown keys.
//!
//! Example layout (one line per event):
//!
//! ```json
//! {"ts":1714200000.123,"ev":"beacon_recv","server":"10.0.0.5:5064","seq":42,"version":13}
//! {"ts":1714200001.456,"ev":"client_connect","peer":"10.0.0.6:54311"}
//! {"ts":1714200002.000,"ev":"client_disconnect","peer":"10.0.0.6:54311"}
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// One recorded event. `Beacon` carries enough state to reconstruct
/// connection topology; `Connect` / `Disconnect` capture per-client
/// lifetime.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordedEvent {
    /// CA beacon (UDP) arrived from `server`.
    BeaconRecv {
        ts: f64,
        server: SocketAddr,
        seq: u32,
        version: u16,
    },
    /// TCP client opened a connection to the server.
    ClientConnect { ts: f64, peer: SocketAddr },
    /// TCP client closed the connection.
    ClientDisconnect { ts: f64, peer: SocketAddr },
}

impl RecordedEvent {
    fn now_ts() -> f64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    pub fn beacon(server: SocketAddr, seq: u32, version: u16) -> Self {
        Self::BeaconRecv {
            ts: Self::now_ts(),
            server,
            seq,
            version,
        }
    }
    pub fn connect(peer: SocketAddr) -> Self {
        Self::ClientConnect {
            ts: Self::now_ts(),
            peer,
        }
    }
    pub fn disconnect(peer: SocketAddr) -> Self {
        Self::ClientDisconnect {
            ts: Self::now_ts(),
            peer,
        }
    }

    pub fn ts(&self) -> f64 {
        match self {
            Self::BeaconRecv { ts, .. }
            | Self::ClientConnect { ts, .. }
            | Self::ClientDisconnect { ts, .. } => *ts,
        }
    }

    /// Render as a single JSON line (no trailing newline).
    pub fn to_json(&self) -> String {
        match self {
            Self::BeaconRecv {
                ts,
                server,
                seq,
                version,
            } => format!(
                "{{\"ts\":{:.3},\"ev\":\"beacon_recv\",\"server\":\"{server}\",\"seq\":{seq},\"version\":{version}}}",
                ts
            ),
            Self::ClientConnect { ts, peer } => format!(
                "{{\"ts\":{:.3},\"ev\":\"client_connect\",\"peer\":\"{peer}\"}}",
                ts
            ),
            Self::ClientDisconnect { ts, peer } => format!(
                "{{\"ts\":{:.3},\"ev\":\"client_disconnect\",\"peer\":\"{peer}\"}}",
                ts
            ),
        }
    }

    /// Parse one JSON line. Tolerant of unknown fields — anything we
    /// don't recognize is skipped, so future extensions don't break
    /// older replayers.
    pub fn from_json(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let ts = json_field_f64(line, "ts")?;
        let ev = json_field_str(line, "ev")?;
        match ev.as_str() {
            "beacon_recv" => Some(Self::BeaconRecv {
                ts,
                server: json_field_str(line, "server")?.parse().ok()?,
                seq: json_field_u64(line, "seq")? as u32,
                version: json_field_u64(line, "version")? as u16,
            }),
            "client_connect" => Some(Self::ClientConnect {
                ts,
                peer: json_field_str(line, "peer")?.parse().ok()?,
            }),
            "client_disconnect" => Some(Self::ClientDisconnect {
                ts,
                peer: json_field_str(line, "peer")?.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// Append-only on-disk recorder. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct EventRecorder {
    file: Arc<Mutex<tokio::fs::File>>,
}

impl EventRecorder {
    pub async fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            file: Arc::new(Mutex::new(f)),
        })
    }

    pub async fn record(&self, ev: &RecordedEvent) {
        let line = ev.to_json();
        let mut f = self.file.lock().await;
        let _ = f.write_all(line.as_bytes()).await;
        let _ = f.write_all(b"\n").await;
    }

    pub async fn flush(&self) {
        let mut f = self.file.lock().await;
        let _ = f.flush().await;
    }
}

/// Stream a recording back through a callback. Honours wall-clock
/// pacing when `paced=true` so a 1-hour recording takes 1 hour to
/// replay; pass `false` to drain as fast as possible (useful for
/// regression tests).
pub async fn replay(
    path: impl AsRef<Path>,
    paced: bool,
    mut sink: impl FnMut(&RecordedEvent),
) -> std::io::Result<usize> {
    let f = tokio::fs::File::open(path).await?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    let mut count = 0usize;
    let mut prior_ts: Option<f64> = None;
    let start = std::time::Instant::now();
    let start_ts: Option<f64> = None;
    let mut start_ts = start_ts;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let Some(ev) = RecordedEvent::from_json(&line) else {
            continue;
        };
        if paced {
            let st = *start_ts.get_or_insert(ev.ts());
            let target = std::time::Duration::from_secs_f64((ev.ts() - st).max(0.0));
            let elapsed = start.elapsed();
            if target > elapsed {
                tokio::time::sleep(target - elapsed).await;
            }
            prior_ts = Some(ev.ts());
        } else {
            let _ = prior_ts;
        }
        sink(&ev);
        count += 1;
    }
    Ok(count)
}

// ── tiny JSON helpers ────────────────────────────────────────────────
//
// The recording format is fixed and small enough that pulling in
// serde_json would be overkill. These helpers extract one field per
// call by string scan; they assume well-formed input as written by
// `to_json`. Callers that need full JSON should swap in serde_json.

fn json_field_str(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_field_f64(line: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_field_u64(line: &str, key: &str) -> Option<u64> {
    let f = json_field_f64(line, key)?;
    Some(f as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_beacon() {
        let ev = RecordedEvent::BeaconRecv {
            ts: 1234.567,
            server: "10.0.0.5:5064".parse().unwrap(),
            seq: 42,
            version: 13,
        };
        let s = ev.to_json();
        let back = RecordedEvent::from_json(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn round_trip_connect() {
        let ev = RecordedEvent::ClientConnect {
            ts: 99.0,
            peer: "10.0.0.6:54311".parse().unwrap(),
        };
        let back = RecordedEvent::from_json(&ev.to_json()).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn unknown_event_returns_none() {
        let line = r#"{"ts":1.0,"ev":"unknown"}"#;
        assert!(RecordedEvent::from_json(line).is_none());
    }

    #[tokio::test]
    async fn record_then_replay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.jsonl");
        let rec = EventRecorder::create(&path).await.unwrap();
        rec.record(&RecordedEvent::beacon(
            "10.0.0.5:5064".parse().unwrap(),
            1,
            13,
        ))
        .await;
        rec.record(&RecordedEvent::connect("10.0.0.6:54311".parse().unwrap()))
            .await;
        rec.flush().await;
        drop(rec);

        let mut seen: Vec<RecordedEvent> = Vec::new();
        let n = replay(&path, false, |ev| seen.push(ev.clone()))
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(matches!(seen[0], RecordedEvent::BeaconRecv { .. }));
        assert!(matches!(seen[1], RecordedEvent::ClientConnect { .. }));
    }
}
