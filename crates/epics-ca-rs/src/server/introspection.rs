//! Tiny HTTP introspection endpoint for the CA server.
//!
//! Exposes a handful of read-only routes so an external supervisor
//! (Kubernetes liveness/readiness probe, Grafana scraper, oncall
//! dashboard) can answer "is this IOC alive, and what is it doing?"
//! without speaking CA. Intentionally minimal — we do not want a
//! heavyweight HTTP framework in the IOC's runtime dependency graph,
//! so the implementation is plain tokio + manual HTTP/1.1 line
//! parsing. Suitable for the scale this serves: low QPS, friendly
//! callers.
//!
//! Routes:
//! - `GET /healthz`   — 200 OK if the server is up. Smoke for probes.
//! - `GET /info`      — JSON: port, uptime, build version
//! - `GET /clients`   — JSON list of currently-connected peer addrs
//! - `GET /queues`    — JSON: configured per-client caps and limits
//!
//! Anything else returns 404. The endpoint is bind-protected: bind to
//! `127.0.0.1:<port>` for IOC-local probes, or `0.0.0.0:<port>` if
//! you've cleared the network policy explicitly. There is no auth —
//! treat this like `/proc`.

// RTEMS-EXEC-MODEL-ALLOW(1): end_to_end_healthz binds a tokio::net TCP
// listener for a real GET, which needs the reactor. These run and pass in the
// feature-ON suite on the tokio driver.
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// Live-updating snapshot the HTTP handlers read from. Concurrent
/// writers (the TCP listener task) just bump the atomic counters; the
/// peer list is mutex-guarded but only touched on connect/disconnect.
pub struct IntrospectionState {
    /// CA TCP listener port.
    pub ca_port: u16,
    /// Server start instant — used for `uptime_secs`.
    pub started: Instant,
    pub clients_active: AtomicU64,
    pub channels_active: AtomicU64,
    pub peers: Mutex<Vec<SocketAddr>>,
    /// Configured limits, surfaced verbatim by /queues.
    pub max_channels_per_client: u64,
    pub max_subs_per_channel: u64,
    pub rate_limit_msgs_per_sec: u64,
    pub rate_limit_burst: u64,
    /// Drain flag, shared with the TCP listener. POST /drain flips it.
    pub drain: Arc<std::sync::atomic::AtomicBool>,
    /// Hook for POST /reload-acf. None when the server has no ACF
    /// source path registered.
    pub reload_acf: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    /// Hook for POST /reload-tls. None when the server has no TLS
    /// source paths registered.
    pub reload_tls: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    /// Optional shared-secret token. When `Some`, every POST request
    /// must include `X-Reload-Token: <token>` (case-insensitive
    /// header name) or the server replies 401. When `None`,
    /// authenticated mutations are allowed unauthenticated — keep
    /// the bind on loopback or trusted admin networks in that case.
    pub reload_token: Option<String>,
    /// Concurrent-connection cap. Slowloris guard so a peer that
    /// dribbles bytes (or completes TCP and never sends any) can't
    /// pin spawned tasks indefinitely.
    pub conn_semaphore: Arc<tokio::sync::Semaphore>,
}

impl IntrospectionState {
    pub fn new(ca_port: u16) -> Arc<Self> {
        Arc::new(Self {
            ca_port,
            started: Instant::now(),
            clients_active: AtomicU64::new(0),
            channels_active: AtomicU64::new(0),
            peers: Mutex::new(Vec::new()),
            max_channels_per_client: 0,
            max_subs_per_channel: 0,
            rate_limit_msgs_per_sec: 0,
            rate_limit_burst: 0,
            drain: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reload_acf: None,
            reload_tls: None,
            reload_token: None,
            // 32 concurrent admin requests is generous for what is in
            // practice a probe / dashboard endpoint. Above that we
            // 503 instead of accepting and queueing.
            conn_semaphore: Arc::new(tokio::sync::Semaphore::new(32)),
        })
    }

    /// Set a shared-secret token required for POST routes
    /// (`/drain`, `/reload-acf`, `/reload-tls`). Without this, any
    /// process that can reach the bind address can mutate live
    /// server config.
    ///
    /// Returns the original Arc unchanged if the Arc has already
    /// been cloned (configuration must precede sharing). Logs a
    /// warning instead of panicking — this avoids the previous
    /// `Arc::get_mut(&mut self).expect(...)` panic on shared Arc.
    pub fn with_reload_token(mut self: Arc<Self>, token: String) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.reload_token = Some(token);
        } else {
            tracing::warn!(
                "IntrospectionState::with_reload_token: Arc already shared; ignoring \
                 (configure builders BEFORE sharing the Arc)"
            );
        }
        self
    }

    /// Builder-style: set the shared drain flag (so POST /drain
    /// signals the same bool the TCP listener checks).
    pub fn with_drain(mut self: Arc<Self>, drain: Arc<std::sync::atomic::AtomicBool>) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.drain = drain;
        } else {
            tracing::warn!("IntrospectionState::with_drain: Arc already shared; ignoring");
        }
        self
    }

    pub fn with_reload_acf(
        mut self: Arc<Self>,
        f: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.reload_acf = Some(f);
        } else {
            tracing::warn!("IntrospectionState::with_reload_acf: Arc already shared; ignoring");
        }
        self
    }

    pub fn with_reload_tls(
        mut self: Arc<Self>,
        f: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Arc<Self> {
        if let Some(inner) = Arc::get_mut(&mut self) {
            inner.reload_tls = Some(f);
        } else {
            tracing::warn!("IntrospectionState::with_reload_tls: Arc already shared; ignoring");
        }
        self
    }

    pub async fn add_peer(&self, peer: SocketAddr) {
        self.clients_active.fetch_add(1, Ordering::AcqRel);
        let mut p = self.peers.lock().await;
        p.push(peer);
    }

    pub async fn remove_peer(&self, peer: SocketAddr) {
        self.clients_active.fetch_sub(1, Ordering::AcqRel);
        let mut p = self.peers.lock().await;
        if let Some(idx) = p.iter().position(|&a| a == peer) {
            p.swap_remove(idx);
        }
    }

    pub fn add_channel(&self) {
        self.channels_active.fetch_add(1, Ordering::AcqRel);
    }

    pub fn remove_channel(&self) {
        self.channels_active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bind the HTTP introspection listener and serve until the listener
/// is dropped. Spawn this on a tokio task; cancellation is by
/// dropping the JoinHandle.
pub async fn run_introspection(
    addr: SocketAddr,
    state: Arc<IntrospectionState>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(bind = %addr, "introspection HTTP server listening");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "introspection accept failed");
                continue;
            }
        };
        // Concurrency cap. Without this, a peer opening N
        // trickle connections holds N tasks for the per-line read
        // timeout × header count = ~2.5 minutes each. The semaphore
        // permit lives for the duration of the spawned handler.
        let permit = match state.conn_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(peer = %peer, "introspection: rejecting (semaphore full)");
                let mut s = stream;
                let _ = write_response_raw(
                    &mut s,
                    503,
                    "Service Unavailable",
                    "{\"error\":\"too many connections\"}",
                )
                .await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // released on task drop
            if let Err(e) = handle_request(stream, state).await {
                tracing::debug!(peer = %peer, error = %e, "introspection request error");
            }
        });
    }
}

async fn write_response_raw(
    stream: &mut TcpStream,
    status: u16,
    text: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await
}

async fn handle_request(stream: TcpStream, state: Arc<IntrospectionState>) -> std::io::Result<()> {
    // Slowloris guard: a peer that completes TCP and never sends bytes
    // (or trickles them one byte at a time) would otherwise pin a
    // tokio task forever. 5 s is generous for a local-admin HTTP
    // request; reject silently on timeout.
    const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_HEADERS: usize = 32;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let read_request_line =
        tokio::time::timeout(HEADER_READ_TIMEOUT, reader.read_line(&mut request_line))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "request line timeout")
            })??;
    if read_request_line == 0 {
        return Ok(());
    }
    // Capture headers so we can check X-Reload-Token on POST routes.
    // RFC 7230 requires reading until the empty line.
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        if headers.len() >= MAX_HEADERS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "too many headers",
            ));
        }
        let mut header = String::new();
        let read_n = tokio::time::timeout(HEADER_READ_TIMEOUT, reader.read_line(&mut header))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "header read timeout")
            })??;
        if read_n == 0 {
            break;
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((
                name.trim().to_ascii_lowercase(),
                value.trim().trim_end_matches(['\r', '\n']).to_string(),
            ));
        }
    }

    let (method, path) = parse_request_line(&request_line);
    let (status, body) = match (method, path) {
        ("GET", "/healthz") => (200, "{\"status\":\"ok\"}".to_string()),
        ("GET", "/info") => (200, render_info(&state)),
        ("GET", "/clients") => (200, render_clients(&state).await),
        ("GET", "/queues") => (200, render_queues(&state)),
        ("POST", route) if matches!(route, "/drain" | "/reload-acf" | "/reload-tls") => {
            // Authn. When reload_token is configured, every
            // mutating POST must present X-Reload-Token. Without
            // configuration the routes fall through to the legacy
            // unauthenticated path; bind on loopback or trusted
            // admin nets in that case.
            let token_ok = match &state.reload_token {
                Some(expected) => headers
                    .iter()
                    .find(|(k, _)| k == "x-reload-token")
                    .map(|(_, v)| v == expected)
                    .unwrap_or(false),
                None => true,
            };
            if !token_ok {
                (401, "{\"error\":\"unauthorized\"}".to_string())
            } else {
                match route {
                    "/drain" => {
                        state
                            .drain
                            .store(true, std::sync::atomic::Ordering::Release);
                        metrics::counter!("ca_server_drain_total").increment(1);
                        (200, "{\"drain\":true}".to_string())
                    }
                    "/reload-acf" => match state.reload_acf.clone() {
                        Some(f) => {
                            // closure does blocking std::fs reads;
                            // run on the blocking pool so a slow NFS
                            // mount doesn't freeze the worker.
                            match tokio::task::spawn_blocking(move || f()).await {
                                Ok(Ok(())) => (200, "{\"reload_acf\":\"ok\"}".to_string()),
                                Ok(Err(e)) => {
                                    (500, format!("{{\"error\":\"{}\"}}", escape_json(&e)))
                                }
                                Err(e) => (
                                    500,
                                    format!("{{\"error\":\"{}\"}}", escape_json(&e.to_string())),
                                ),
                            }
                        }
                        None => (501, "{\"error\":\"reload_acf not configured\"}".to_string()),
                    },
                    "/reload-tls" => match state.reload_tls.clone() {
                        Some(f) => match tokio::task::spawn_blocking(move || f()).await {
                            Ok(Ok(())) => (200, "{\"reload_tls\":\"ok\"}".to_string()),
                            Ok(Err(e)) => (500, format!("{{\"error\":\"{}\"}}", escape_json(&e))),
                            Err(e) => (
                                500,
                                format!("{{\"error\":\"{}\"}}", escape_json(&e.to_string())),
                            ),
                        },
                        None => (501, "{\"error\":\"reload_tls not configured\"}".to_string()),
                    },
                    _ => unreachable!(),
                }
            }
        }
        ("GET", _) | ("POST", _) => (404, "{\"error\":\"not_found\"}".to_string()),
        _ => return write_response(reader.into_inner(), 405, "Method Not Allowed", "").await,
    };
    write_response(reader.into_inner(), status, status_text(status), &body).await
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn parse_request_line(line: &str) -> (&str, &str) {
    let line = line.trim_end();
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    (method, path)
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

async fn write_response(
    mut stream: TcpStream,
    code: u16,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn render_info(state: &IntrospectionState) -> String {
    let uptime = state.started.elapsed().as_secs();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{{\"ca_port\":{},\"uptime_secs\":{},\"now_unix\":{},\"version\":\"{}\",\"clients\":{},\"channels\":{}}}",
        state.ca_port,
        uptime,
        now,
        env!("CARGO_PKG_VERSION"),
        state.clients_active.load(Ordering::Acquire),
        state.channels_active.load(Ordering::Acquire),
    )
}

async fn render_clients(state: &IntrospectionState) -> String {
    let peers = state.peers.lock().await;
    let mut s = String::from("{\"clients\":[");
    for (i, p) in peers.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(&p.to_string());
        s.push('"');
    }
    s.push_str("]}");
    s
}

fn render_queues(state: &IntrospectionState) -> String {
    format!(
        "{{\"max_channels_per_client\":{},\"max_subs_per_channel\":{},\"rate_limit_msgs_per_sec\":{},\"rate_limit_burst\":{}}}",
        state.max_channels_per_client,
        state.max_subs_per_channel,
        state.rate_limit_msgs_per_sec,
        state.rate_limit_burst,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_line_basic() {
        let (m, p) = parse_request_line("GET /healthz HTTP/1.1\r\n");
        assert_eq!(m, "GET");
        assert_eq!(p, "/healthz");
    }

    #[test]
    fn render_info_contains_expected_fields() {
        let s = IntrospectionState::new(5064);
        s.clients_active.store(3, Ordering::Release);
        let body = render_info(&s);
        assert!(body.contains("\"ca_port\":5064"));
        assert!(body.contains("\"clients\":3"));
        assert!(body.contains("\"version\":"));
    }

    #[epics_macros_rs::epics_test]
    async fn render_clients_handles_empty() {
        let s = IntrospectionState::new(5064);
        assert_eq!(render_clients(&s).await, "{\"clients\":[]}");
    }

    #[tokio::test]
    async fn end_to_end_healthz() {
        // Bind ephemeral port, fire a real GET /healthz, parse the
        // response. Catches issues with the request reader/writer
        // that the unit-level tests don't exercise.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // re-bind inside run_introspection
        let state = IntrospectionState::new(5064);
        let st = state.clone();
        let server = tokio::spawn(async move {
            let _ = run_introspection(addr, st).await;
        });
        // give the listener a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 256];
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&buf[..n]);
        assert!(s.contains("200 OK"));
        assert!(s.contains("\"status\":\"ok\""));
        server.abort();
    }
}
