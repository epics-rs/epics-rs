//! Talk to a running CA server's introspection HTTP endpoint.
//!
//! Mirrors the routes exposed by `server::introspection`:
//!
//! ```bash
//! ca-admin-rs --host 127.0.0.1:9100 status        # GET /info
//! ca-admin-rs --host 127.0.0.1:9100 clients       # GET /clients
//! ca-admin-rs --host 127.0.0.1:9100 drain         # POST /drain
//! ca-admin-rs --host 127.0.0.1:9100 reload-acf    # POST /reload-acf
//! ```
//!
//! Self-contained HTTP/1.1 client (no `reqwest`) so the binary is
//! tiny and copies cleanly into containers.

use clap::{Parser, Subcommand};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Parser)]
#[command(name = "ca-admin-rs")]
struct Args {
    /// Introspection endpoint, e.g. `127.0.0.1:9100`. Falls back to
    /// `EPICS_CAS_INTROSPECTION_ADDR`.
    #[arg(long)]
    host: Option<String>,

    /// HTTP timeout in seconds. Default 5.
    #[arg(long, default_value_t = 5)]
    timeout: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// GET /healthz — quick liveness probe.
    Healthz,
    /// GET /info — version, uptime, counts.
    Status,
    /// GET /clients — current peer list.
    Clients,
    /// GET /queues — configured limits.
    Queues,
    /// POST /drain — start graceful drain.
    Drain,
    /// POST /reload-acf — re-read the ACF source path.
    ReloadAcf,
    /// POST /reload-tls — re-read the TLS cert/key paths and swap
    /// the active config atomically. Useful before cert expiry.
    ReloadTls,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let host = match args.host {
        Some(h) => h,
        None => std::env::var("EPICS_CAS_INTROSPECTION_ADDR")
            .map_err(|_| "no --host and EPICS_CAS_INTROSPECTION_ADDR is unset")?,
    };
    let (method, path) = match args.cmd {
        Cmd::Healthz => ("GET", "/healthz"),
        Cmd::Status => ("GET", "/info"),
        Cmd::Clients => ("GET", "/clients"),
        Cmd::Queues => ("GET", "/queues"),
        Cmd::Drain => ("POST", "/drain"),
        Cmd::ReloadAcf => ("POST", "/reload-acf"),
        Cmd::ReloadTls => ("POST", "/reload-tls"),
    };

    let timeout = Duration::from_secs(args.timeout);
    let (status, body) =
        epics_base_rs::runtime::task::timeout(timeout, do_request(&host, method, path)).await??;
    println!("{body}");
    if !(200..300).contains(&status) {
        std::process::exit(1);
    }
    Ok(())
}

async fn do_request(
    host: &str,
    method: &str,
    path: &str,
) -> Result<(u16, String), Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect(host).await?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = parse_status(&text).unwrap_or(0);
    let body = match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => text,
    };
    Ok((status, body))
}

fn parse_status(text: &str) -> Option<u16> {
    let line = text.lines().next()?;
    let mut parts = line.split(' ');
    let _version = parts.next()?;
    parts.next()?.parse().ok()
}
